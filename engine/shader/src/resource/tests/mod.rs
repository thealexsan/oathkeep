mod basic;
mod per_lane;
mod select;
mod untraceable;
mod waterfall;

pub(crate) use per_lane::cmpx_waterfall;
pub(crate) use untraceable::{
    WATERFALL_FETCHED_WAVE32, WATERFALL_FETCHED_WAVE64, WATERFALL_INDEXED_WAVE32,
    WATERFALL_JOINED_WAVE32, WATERFALL_PICKED_WAVE32,
};
pub(crate) use waterfall::{
    WATERFALL_JOINED_BUFFER_WAVE32, WATERFALL_JOINED_MIXED_WAVE32, WATERFALL_LOOPED_WAVE32,
};

use crate::ir::Program;
use crate::native::{HwStage, RootParam, RootType};
use crate::structure::Structured;
use super::*;

pub(super) const S_ENDPGM: u32 = 0xBF81_0000;

pub(super) fn track_words(words: &[u32], layout: &[RootParam], stage: HwStage) -> Resources {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = crate::ir::build(&code, false, 256).unwrap();
    track(&p, layout, stage, lanes(&p).as_ref()).unwrap()
}

/// `p` as one lane runs it, if it structures.
pub(super) fn lanes(p: &Program) -> Option<Structured> {
    crate::structure::structure(p).ok()
}

pub(super) fn param(root: RootType, user_data: u8, sgprs: u32) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot: 0,
    }
}

pub(super) fn at(param: usize, offset: u32) -> Address {
    Address {
        param,
        offset,
        dynamic: None,
    }
}

