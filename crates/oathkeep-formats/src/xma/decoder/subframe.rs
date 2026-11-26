use super::Decoder;
use crate::xma::bits::{Bits, codes, large_value, log2};
use crate::xma::imdct::{imdct_half, window_overlap, windows};
use crate::xma::tables::*;
use crate::xma::{Error, FRAME_SAMPLES, MAX_SUBFRAMES};

impl Decoder {
    pub(super) fn coefficients(&mut self, b: &mut Bits, c: usize) -> Result<(), Error> {
        let codes = codes();
        let table = b.bit() as usize;
        let (run, level): (&[u16], &[f32]) = if table == 1 {
            (&COEF1_RUN, &COEF1_LEVEL)
        } else {
            (&COEF0_RUN, &COEF0_LEVEL)
        };
        let subframe_len = self.subframe_len;
        let ch = &mut self.ch[c];
        let mut rl_mode = false;
        let mut cur = 0;
        let mut num_zeros = 0;
        while (self.transmit_num_vec_coeffs || !rl_mode) && cur + 3 < ch.num_vec_coeffs {
            let mut vals = [0u32; 4];
            let idx = codes.vec4.read(b);
            if idx < 0 {
                for i in (0..4).step_by(2) {
                    let idx = codes.vec2.read(b);
                    if idx < 0 {
                        for v in &mut vals[i..i + 2] {
                            let mut x = codes.vec1.read(b) as u32;
                            if x as usize == VEC1.len() - 1 {
                                x += large_value(b);
                            }
                            *v = x;
                        }
                    } else {
                        vals[i] = (idx >> 4) as u32;
                        vals[i + 1] = (idx & 0xf) as u32;
                    }
                }
            } else {
                vals = [
                    (idx >> 12) as u32,
                    ((idx >> 8) & 0xf) as u32,
                    ((idx >> 4) & 0xf) as u32,
                    (idx & 0xf) as u32,
                ];
            }
            for v in vals {
                if v != 0 {
                    let negative = !b.bit();
                    ch.coeffs[cur] = if negative { -(v as f32) } else { v as f32 };
                    num_zeros = 0;
                } else {
                    ch.coeffs[cur] = 0.0;
                    num_zeros += 1;
                    rl_mode |= num_zeros > subframe_len >> 8;
                }
                cur += 1;
            }
            if b.overread() {
                return Err(Error("vector coefficients"));
            }
        }
        if cur < subframe_len {
            ch.coeffs[cur..subframe_len].fill(0.0);
            // `ff_wma_run_level_decode`, version 1.
            let mask = subframe_len - 1;
            let mut offset = cur;
            while offset < subframe_len {
                let code = codes.coef[table].read(b);
                if code > 1 {
                    offset += run[code as usize] as usize;
                    let negative = !b.bit();
                    let v = level[code as usize];
                    ch.coeffs[offset & mask] = if negative { -v } else { v };
                } else if code == 1 {
                    break;
                } else {
                    let value = large_value(b) as i32;
                    if b.bit() {
                        if b.bit() {
                            if b.bit() {
                                // A broken escape ends the channel's
                                // coefficients, as in FFmpeg.
                                break;
                            }
                            offset += b.read(self.esc_len) as usize + 4;
                        } else {
                            offset += b.read(2) as usize + 1;
                        }
                    }
                    let negative = !b.bit();
                    ch.coeffs[offset & mask] = if negative { -value } else { value } as f32;
                }
                if b.overread() {
                    return Err(Error("run level coefficients"));
                }
                offset += 1;
            }
        }
        Ok(())
    }

    pub(super) fn scale_factors(&mut self, b: &mut Bits) -> Result<(), Error> {
        let codes = codes();
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            let sf_offsets = self.sf_offsets[self.table_idx][self.ch[c].table_idx];
            let ch = &mut self.ch[c];
            let target = 1 - ch.scale_factor_idx;
            ch.scale_factors = target;
            if ch.reuse_sf {
                let from = ch.saved_scale_factors[ch.scale_factor_idx];
                for (sf, &offset) in ch.saved_scale_factors[target].iter_mut().zip(&sf_offsets).take(self.num_bands) {
                    *sf = from[offset];
                }
            }
            if ch.cur_subframe == 0 || b.bit() {
                if !ch.reuse_sf {
                    ch.scale_factor_step = b.read(2) as i32 + 1;
                    let mut val = 45 / ch.scale_factor_step;
                    for band in 0..self.num_bands {
                        val += codes.scale.read(b);
                        ch.saved_scale_factors[target][band] = val;
                    }
                } else {
                    let mut band = 0;
                    while band < self.num_bands {
                        let idx = codes.scale_rl.read(b);
                        let (skip, val, negative) = match idx {
                            0 => {
                                let code = b.read(14);
                                ((code & 0x3f) >> 1, (code >> 6) as i32, code & 1 == 0)
                            }
                            1 => break,
                            _ => (
                                SCALE_RL_RUN[idx as usize] as u32,
                                SCALE_RL_LEVEL[idx as usize] as i32,
                                !b.bit(),
                            ),
                        };
                        band += skip as usize;
                        if band >= self.num_bands {
                            return Err(Error("scale factor run"));
                        }
                        ch.saved_scale_factors[target][band] += if negative { -val } else { val };
                        band += 1;
                    }
                }
                ch.scale_factor_idx = target;
                ch.table_idx = self.table_idx;
                ch.reuse_sf = true;
            }
            let sf = &ch.saved_scale_factors[ch.scale_factors][..self.num_bands];
            ch.max_scale_factor = sf.iter().copied().max().unwrap_or(0);
        }
        Ok(())
    }

    pub(super) fn subframe(&mut self, b: &mut Bits) -> Result<(), Error> {
        let mut offset = FRAME_SAMPLES;
        let mut subframe_len = FRAME_SAMPLES;
        let mut total_samples = FRAME_SAMPLES * self.channels;
        for c in 0..self.channels {
            let ch = &self.ch[c];
            if offset > ch.decoded_samples {
                offset = ch.decoded_samples;
                subframe_len = ch.subframe_len[ch.cur_subframe.min(MAX_SUBFRAMES - 1)];
            }
        }
        self.channels_for_cur_subframe = 0;
        for c in 0..self.channels {
            let ch = &mut self.ch[c];
            total_samples -= ch.decoded_samples;
            if offset == ch.decoded_samples
                && ch.cur_subframe < ch.num_subframes
                && subframe_len == ch.subframe_len[ch.cur_subframe]
            {
                total_samples -= subframe_len;
                ch.decoded_samples += subframe_len;
                self.channel_indexes[self.channels_for_cur_subframe] = c;
                self.channels_for_cur_subframe += 1;
            }
        }
        if self.channels_for_cur_subframe == 0 {
            return Err(Error("no channel in subframe"));
        }
        if total_samples == 0 {
            self.parsed_all_subframes = true;
        }
        self.table_idx = log2(FRAME_SAMPLES / subframe_len) as usize;
        self.num_bands = self.num_sfb[self.table_idx];
        self.subframe_len = subframe_len;
        self.esc_len = log2(subframe_len - 1) + 1;

        // Extended header: fill bits.
        if b.bit() {
            let mut fill = b.read(2) as usize;
            if fill == 0 {
                let len = b.read(4);
                fill = b.read(len) as usize + 1;
            }
            b.pos += fill;
        }
        if b.bit() {
            return Err(Error("reserved bit"));
        }
        self.channel_transform(b)?;

        let mut transmit_coeffs = false;
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            self.ch[c].transmit_coefs = b.bit();
            transmit_coeffs |= self.ch[c].transmit_coefs;
        }
        if transmit_coeffs {
            self.transmit_num_vec_coeffs = b.bit();
            if self.transmit_num_vec_coeffs {
                let num_bits = log2(subframe_len.div_ceil(4)) + 1;
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes[i];
                    let n = (b.read(num_bits) << 2) as usize;
                    if n > subframe_len {
                        return Err(Error("vector coefficient count"));
                    }
                    self.ch[c].num_vec_coeffs = n;
                }
            } else {
                for i in 0..self.channels_for_cur_subframe {
                    self.ch[self.channel_indexes[i]].num_vec_coeffs = subframe_len;
                }
            }
            // Quantisation step (16-bit samples: 90 * 16 >> 4).
            let mut quant_step = 90;
            let mut step = b.signed(6);
            quant_step += step;
            if step == -32 || step == 31 {
                let negative = step == -32;
                let mut quant = 0;
                while b.pos + 5 < b.end {
                    step = b.read(5) as i32;
                    if step != 31 {
                        break;
                    }
                    quant += 31;
                }
                quant_step += if negative { -(quant + step) } else { quant + step };
            }
            if self.channels_for_cur_subframe == 1 {
                self.ch[self.channel_indexes[0]].quant_step = quant_step;
            } else {
                let modifier_len = b.read(3);
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes[i];
                    self.ch[c].quant_step = quant_step;
                    if b.bit() {
                        self.ch[c].quant_step += if modifier_len > 0 {
                            b.read(modifier_len) as i32 + 1
                        } else {
                            1
                        };
                    }
                }
            }
            self.scale_factors(b)?;
        }

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes[i];
            if self.ch[c].transmit_coefs && b.pos < b.end {
                self.coefficients(b, c)?;
            } else {
                self.ch[c].coeffs[..subframe_len].fill(0.0);
            }
        }

        // Where this subframe's samples go in the channel output.
        let at = offset + FRAME_SAMPLES / 2;
        if transmit_coeffs {
            self.inverse_channel_transform();
            // Output in [-1, 1] for 16-bit streams.
            let scale = 2.0 / subframe_len as f32 / 32768.0;
            let sfb = self.sfb_offsets[self.table_idx];
            let mut tmp = [0f32; FRAME_SAMPLES];
            for i in 0..self.channels_for_cur_subframe {
                let ch = &mut self.ch[self.channel_indexes[i]];
                let sf = ch.saved_scale_factors[ch.scale_factors];
                for band in 0..self.num_bands {
                    let end = sfb[band + 1].min(subframe_len);
                    let exp = ch.quant_step - (ch.max_scale_factor - sf[band]) * ch.scale_factor_step;
                    let quant = 10f32.powf(exp as f32 / 20.0);
                    for (t, &c) in tmp[sfb[band]..end].iter_mut().zip(&ch.coeffs[sfb[band]..end]) {
                        *t = c * quant;
                    }
                }
                imdct_half(&tmp[..subframe_len], &mut ch.out[at..at + subframe_len], scale);
            }
        } else {
            for i in 0..self.channels_for_cur_subframe {
                self.ch[self.channel_indexes[i]].out[at..at + subframe_len].fill(0.0);
            }
        }

        // Window and overlap-add with the previous block.
        for i in 0..self.channels_for_cur_subframe {
            let ch = &mut self.ch[self.channel_indexes[i]];
            let mut winlen = ch.prev_block_len;
            let mut start = at - winlen / 2;
            if subframe_len < winlen {
                start += (winlen - subframe_len) / 2;
                winlen = subframe_len;
            }
            window_overlap(&mut ch.out[start..start + winlen], &windows()[log2(winlen) as usize - 7]);
            ch.prev_block_len = subframe_len;
        }

        for i in 0..self.channels_for_cur_subframe {
            let ch = &mut self.ch[self.channel_indexes[i]];
            if ch.cur_subframe >= ch.num_subframes {
                return Err(Error("subframe overrun"));
            }
            ch.cur_subframe += 1;
        }
        if b.overread() {
            return Err(Error("subframe overread"));
        }
        Ok(())
    }

    pub(super) fn inverse_channel_transform(&mut self) {
        if !self.transform || self.channels_for_cur_subframe != 2 {
            return;
        }
        let sfb = self.sfb_offsets[self.table_idx];
        let [a, b] = &mut self.ch;
        for band in 0..self.num_bands {
            let range = sfb[band]..sfb[band + 1].min(self.subframe_len);
            if self.transform_band[band] {
                for y in range {
                    let (l, r) = (a.coeffs[y], b.coeffs[y]);
                    (a.coeffs[y], b.coeffs[y]) = (l - r, l + r);
                }
            } else {
                for y in range {
                    a.coeffs[y] *= 181.0 / 128.0;
                    b.coeffs[y] *= 181.0 / 128.0;
                }
            }
        }
    }
}
