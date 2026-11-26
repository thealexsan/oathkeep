mod async_call;
mod queue;

use super::vtable::OBJECT;

fn slot<F: Copy>(offset: usize) -> F {
    crate::modules::xgameruntime::method(&OBJECT, offset)
}
