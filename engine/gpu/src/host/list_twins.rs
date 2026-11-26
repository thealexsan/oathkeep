use super::*;

impl HostList {
    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        // Asked for every texture a draw reads: most have no twin.
        let Some(twin) = crate::resource::twin(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
            });
        let was = (u.needs, u.behind, u.writing);
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            // Shaders write the twin when the draw or dispatch runs: what
            // the same one reads of the depth texture is prepared before
            // (`twins_written`).
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        if was != (u.needs, u.behind, u.writing) || copy || colour {
            self.changed_something();
        }
        let inline_plane = copy && !colour && twin.plane.is_some();
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
            if inline_plane {
                self.pass_plane(&depth, &twin);
            }
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        // The depth plane's twin is written through unordered access too.
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    /// Returns `true` (and clears the flag) if an inline depth-plane pass or
    /// depth-to-plane copy-back clobbered this list's pipeline, root
    /// signatures, heaps, or graphics target/rasterizer state.
    #[inline]
    pub(crate) fn take_state_clobbered(&self) -> bool {
        self.state_clobbered.load(Ordering::Relaxed)
            && self.state_clobbered.swap(false, Ordering::AcqRel)
    }

    /// Records the pass that draws the twin of the depth plane of `depth`
    /// into the plane, if `twin` has one, with the moves to the states the
    /// pass needs; the twin is left in the unordered access state and
    /// `state_clobbered` is set so the next draw or dispatch replays state.
    pub(in crate::host) fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
        self.state_clobbered.store(true, Ordering::Release);
    }

    /// Records the compute pass that copies the depth plane of `depth` back
    /// into its twin, leaving the plane twin in `UNORDERED_ACCESS` and
    /// marking `state_clobbered` on success.
    pub(in crate::host) fn depth_to_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DENY_SHADER_RESOURCE: u32 = 0x8;
        const NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
        let Some(plane) = &twin.plane else { return };
        if depth.flags() & DENY_SHADER_RESOURCE != 0 {
            crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.settle(depth, NON_PIXEL_SHADER_RESOURCE);
        self.settle(plane, UNORDERED_ACCESS);
        if record_depth_to_plane(&self.list, &depth.texture, &plane.texture) {
            self.state_clobbered.store(true, Ordering::Release);
        } else {
            crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records the copy of a depth texture into its colour twin (`colour`)
    /// or of the twin into the depth texture, with the moves to the copy
    /// states. The twin is left in the unordered access state.
    pub(in crate::host) fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
        if colour && twin.plane.is_some() {
            self.depth_to_plane(depth, twin);
        }
    }

    /// Before the list closes: the depth textures whose twins it wrote last
    /// are given what was written, and left readable by shaders. Shaders
    /// that pick a view from the heap by a computed index are not seen by
    /// `twin_use` and read the depth texture in lists that never name it
    /// (in a captured frame of the title no table names a shadow
    /// map for reading but the pass that fills the next one).
    pub(in crate::host) fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true))
            .map(|u| {
                u.behind[0] = Some(false);
                (u.depth.clone(), u.twin.clone())
            })
            .collect();
        drop(twins);
        for (depth, twin) in &written {
            self.twin_copy(depth, twin, false);
            // Nothing of the title's is recorded after this: the pass may
            // leave its own pipeline and target set.
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    /// A draw or dispatch being prepared writes title texture `resource`
    /// through unordered access: if it is of a kind copied back
    /// ([`crate::texback`]), the list copies it when it closes.
    pub(crate) fn texture_written(&self, resource: crate::object::This) {
        if !crate::texback::wanted(resource) {
            return;
        }
        let mut deferred = self.deferred();
        if !deferred.back_wanted.iter().any(|target| target.owner() == resource as usize)
            && let Some(target) = crate::texback::Target::new(resource)
        {
            deferred.back_wanted.push(target);
        }
    }

    /// Before the list closes: the textures its shaders wrote that are copied
    /// back go into read-back buffers ([`crate::texback::record`]).
    pub(in crate::host) fn textures_back(&self) {
        const COPY_SOURCE: u32 = 0x800;
        let wanted = std::mem::take(&mut self.deferred().back_wanted);
        for target in wanted {
            self.settle(&target.copy, COPY_SOURCE);
            if let Some(back) = crate::texback::record(&self.list, target) {
                self.deferred().texture_backs.push(back);
            }
        }
    }

    /// The draw or dispatch just prepared is recorded: the twins it writes
    /// now hold what their depth textures lack.
    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
            self.changed_something();
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with. For depth textures with a
    /// colour twin it first copies the side the list uses as it finds it
    /// from the other, if a list submitted earlier left it behind.
    pub(in crate::host) fn prelude(&self) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies (depth texture, twin, whether into the twin); the
        // sides are then as this list leaves them.
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        // A copy runs with its textures in the copy states: each moves
        // there from its real state, and from there to the state the list
        // starts with (back to its real state if the list does not use it).
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            const UNORDERED_ACCESS: u32 = 0x8;
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(transition(
                        &texture.texture,
                        host_state(real),
                        host_state(state),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &texture.texture,
                            host_state(state),
                            host_state(real),
                        ));
                    }
                }
            }
            if let Some(plane) = &twin.plane {
                let key = std::sync::Arc::as_ptr(plane) as usize;
                let real = plane.state.load(Ordering::Relaxed);
                copying.insert(key, UNORDERED_ACCESS);
                if real != UNORDERED_ACCESS {
                    to_copy.push(transition(
                        &plane.texture,
                        host_state(real),
                        host_state(UNORDERED_ACCESS),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &plane.texture,
                            host_state(UNORDERED_ACCESS),
                            host_state(real),
                        ));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first)
                .then(|| transition(&copy.texture, host_state(real), host_state(*first)))
        }));
        if barriers.is_empty() && syncs.is_empty() {
            return None;
        }
        let Some((allocator, list)) = self
            .prelude
            .get_or_init(|| host().and_then(|h| h.make_prelude()))
            .as_ref()
        else {
            fail(format_args!("no host list for a state prelude"));
        };
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    const DENY_SHADER_RESOURCE: u32 = 0x8;
                    record_twin_copy(list, &depth.texture, twin, *colour);
                    if let Some(plane) = &twin.plane {
                        if *colour {
                            if depth.flags() & DENY_SHADER_RESOURCE == 0 {
                                list.ResourceBarrier(&[transition(
                                    &depth.texture,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                                )]);
                                if !record_depth_to_plane(list, &depth.texture, &plane.texture) {
                                    crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
                                }
                                list.ResourceBarrier(&[transition(
                                    &depth.texture,
                                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                                )]);
                            } else {
                                crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            list.ResourceBarrier(&[
                                transition(
                                    &depth.texture,
                                    D3D12_RESOURCE_STATE_COPY_DEST,
                                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                                ),
                                transition(
                                    &plane.texture,
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                                ),
                            ]);
                            record_plane_pass(list, &depth.texture, &plane.texture);
                            list.ResourceBarrier(&[
                                transition(
                                    &plane.texture,
                                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                                ),
                                transition(
                                    &depth.texture,
                                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                                    D3D12_RESOURCE_STATE_COPY_DEST,
                                ),
                            ]);
                        }
                    }
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

}
