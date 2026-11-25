#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};

use super::xasync::{self, AsyncBlock, E_ILLEGAL_METHOD_CALL};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, put_string, text, token,
};
use super::xpackage;
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0x0dd1_12ac,
    0x7c24,
    0x448c,
    [0xb9, 0x2b, 0x39, 0x60, 0xfb, 0x5b, 0xd3, 0x0c],
);
pub const IID_2026: Guid = Guid::new(
    0xb09d_803c,
    0x2414,
    0x4a05,
    [0x82, 0xc6, 0x66, 0xdf, 0xdc, 0x9e, 0x9a, 0x44],
);
pub const DURABLES_IID: Guid = Guid::new(
    0x2d42_fea5,
    0xe71d,
    0x4b76,
    [0x97, 0xcd, 0xc5, 0x0a, 0xfb, 0xb3, 0xae, 0x5d],
);

pub static OBJECT: Object = Object::new("store", &VTABLE);
pub const INTERFACES: &[Interface] = &[
    (PROVIDER, IID_2026, &OBJECT),
    (PROVIDER, DURABLES_IID, &OBJECT),
];

pub const NETWORK_ERROR: HRESULT = 0x8924_5301_u32 as i32;

type This = *const Object;
type Handle = *const c_void;

static CONTEXT: u8 = 0;
static QUERY: u8 = 0;
static VALID: u8 = 0;
static INVALID: u8 = 0;
fn handle(of: &'static u8) -> Handle {
    (of as *const u8).cast()
}

static FAILED: u8 = 0;
static QUERY_CALL: u8 = 0;
static LICENSE_CALL: u8 = 0;
static GAME_LICENSE: u8 = 0;

fn offline(block: *mut AsyncBlock, what: &str) -> HRESULT {
    tracing::info!(what, "store call: offline");
    xasync::finished(block, (&raw const FAILED).cast(), NETWORK_ERROR)
}
extern "system" fn status(_: This, block: *mut AsyncBlock) -> HRESULT {
    xasync::XAsyncGetStatus(block, false)
}

extern "system" fn create_context(_: This, _user: *const c_void, out: *mut Handle) -> HRESULT {
    put(out, handle(&CONTEXT))
}
extern "system" fn close(_: This, _: Handle) {}

extern "system" fn associated_products_async(
    _: This,
    _: Handle,
    _kinds: u32,
    _per_page: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "associated products")
}

extern "system" fn entitled_products_async(
    _: This,
    _: Handle,
    _kinds: u32,
    _per_page: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    xasync::finished_with(
        block,
        (&raw const QUERY_CALL).cast(),
        Ok(xasync::bytes(handle(&QUERY))),
    )
}
extern "system" fn query_result(_: This, block: *mut AsyncBlock, out: *mut Handle) -> HRESULT {
    xasync::result(block, (&raw const QUERY_CALL).cast(), out)
}
extern "system" fn enumerate_products(
    _: This,
    _: Handle,
    _: *mut c_void,
    _: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn has_more_pages(_: This, _: Handle) -> bool {
    false
}
extern "system" fn next_page_async(_: This, _: Handle, block: *mut AsyncBlock) -> HRESULT {
    xasync::finished(block, (&raw const QUERY_CALL).cast(), E_ILLEGAL_METHOD_CALL)
}

fn license_for(owned: bool, block: *mut AsyncBlock) -> HRESULT {
    let license = handle(if owned { &VALID } else { &INVALID });
    xasync::finished_with(
        block,
        (&raw const LICENSE_CALL).cast(),
        Ok(xasync::bytes(license)),
    )
}
extern "system" fn license_for_package_async(
    _: This,
    _: Handle,
    package: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    let ours = text(package).is_some_and(|p| p == xpackage::identifier());
    license_for(ours, block)
}
extern "system" fn license_for_durables_async(
    _: This,
    _: Handle,
    store_id: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        store_id = text(store_id).unwrap_or_default(),
        "durable not owned"
    );
    license_for(false, block)
}
extern "system" fn license_result(_: This, block: *mut AsyncBlock, out: *mut Handle) -> HRESULT {
    xasync::result(block, (&raw const LICENSE_CALL).cast(), out)
}
extern "system" fn is_license_valid(_: This, license: Handle) -> bool {
    license == handle(&VALID)
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GameLicense {
    pub sku_store_id: [u8; 18],
    pub is_active: bool,
    pub is_trial_owned_by_this_user: bool,
    pub is_disc_license: bool,
    pub is_trial: bool,
    pub trial_seconds_remaining: u32,
    pub trial_unique_id: [u8; 64],
    pub expiration: i64,
}

fn game_license() -> GameLicense {
    let mut sku = [0u8; 18];
    let id = format!("{}/0010", runtime::title().store_id);
    let len = id.len().min(sku.len() - 1);
    sku[..len].copy_from_slice(&id.as_bytes()[..len]);
    GameLicense {
        sku_store_id: sku,
        is_active: true,
        is_trial_owned_by_this_user: false,
        is_disc_license: false,
        is_trial: false,
        trial_seconds_remaining: 0,
        trial_unique_id: [0; 64],
        expiration: 0,
    }
}

extern "system" fn game_license_async(_: This, _: Handle, block: *mut AsyncBlock) -> HRESULT {
    let license = xasync::bytes(game_license());
    xasync::finished_with(block, (&raw const GAME_LICENSE).cast(), Ok(license))
}
extern "system" fn game_license_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut GameLicense,
) -> HRESULT {
    xasync::result(block, (&raw const GAME_LICENSE).cast(), out)
}

extern "system" fn consumable_balance_async(
    _: This,
    _: Handle,
    _store_id: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "consumable balance")
}
extern "system" fn report_fulfillment_async(
    _: This,
    _: Handle,
    _store_id: *const c_char,
    _quantity: u32,
    _tracking_id: *const c_void,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "consumable fulfillment")
}
extern "system" fn user_id_async(
    _: This,
    _: Handle,
    _ticket: *const c_char,
    _publisher_user_id: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "user store id")
}
extern "system" fn size_status(_: This, block: *mut AsyncBlock, _out: *mut usize) -> HRESULT {
    xasync::XAsyncGetStatus(block, false)
}
extern "system" fn string_status(
    _: This,
    block: *mut AsyncBlock,
    _: usize,
    _: *mut c_char,
) -> HRESULT {
    xasync::XAsyncGetStatus(block, false)
}
extern "system" fn count_status(_: This, block: *mut AsyncBlock, _out: *mut u32) -> HRESULT {
    xasync::XAsyncGetStatus(block, false)
}
extern "system" fn array_status(
    _: This,
    block: *mut AsyncBlock,
    _: u32,
    _: *mut c_void,
) -> HRESULT {
    xasync::XAsyncGetStatus(block, false)
}

extern "system" fn purchase_ui_async(
    _: This,
    _: Handle,
    _store_id: *const c_char,
    _name: *const c_char,
    _json: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "purchase page")
}
extern "system" fn product_page_ui_async(
    _: This,
    _: Handle,
    _store_id: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "product page")
}
extern "system" fn redeem_ui_async(
    _: This,
    _: Handle,
    _token: *const c_char,
    _allowed: *const *const c_char,
    _allowed_count: usize,
    _disallow_csv: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "redeem page")
}
extern "system" fn updates_async(_: This, _: Handle, block: *mut AsyncBlock) -> HRESULT {
    offline(block, "update check")
}
extern "system" fn install_async(
    _: This,
    _: Handle,
    _ids: *const *const c_char,
    _count: usize,
    block: *mut AsyncBlock,
) -> HRESULT {
    offline(block, "package download")
}

extern "system" fn package_identifier(
    _: This,
    store_id: *const c_char,
    size: usize,
    buffer: *mut u8,
) -> HRESULT {
    let title = runtime::title();
    match text(store_id) {
        Some(id) if !title.store_id.is_empty() && id == title.store_id => {
            put_string(&xpackage::identifier(), size, buffer, std::ptr::null_mut())
        }
        _ => xpackage::NO_PACKAGE,
    }
}

extern "system" fn register(
    _: This,
    _: Handle,
    _queue: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}
extern "system" fn unregister(_: This, _: Handle, _token: u64, _wait: bool) -> bool {
    true
}

static VTABLE: Methods<76> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    create_context as *const (),
    close as *const (),
    associated_products_async as *const (),
    query_result as *const (),
    missing::<7> as *const (),
    missing::<8> as *const (),
    entitled_products_async as *const (),
    query_result as *const (),
    missing::<11> as *const (),
    missing::<12> as *const (),
    missing::<13> as *const (),
    missing::<14> as *const (),
    enumerate_products as *const (),
    has_more_pages as *const (),
    next_page_async as *const (),
    query_result as *const (),
    close as *const (),
    license_for_package_async as *const (),
    license_result as *const (),
    is_license_valid as *const (),
    close as *const (),
    missing::<24> as *const (),
    missing::<25> as *const (),
    missing::<26> as *const (),
    missing::<27> as *const (),
    game_license_async as *const (),
    game_license_result as *const (),
    missing::<30> as *const (),
    missing::<31> as *const (),
    missing::<32> as *const (),
    consumable_balance_async as *const (),
    status as *const (),
    report_fulfillment_async as *const (),
    missing::<36> as *const (),
    user_id_async as *const (),
    size_status as *const (),
    string_status as *const (),
    user_id_async as *const (),
    size_status as *const (),
    string_status as *const (),
    missing::<43> as *const (),
    missing::<44> as *const (),
    missing::<45> as *const (),
    missing::<46> as *const (),
    missing::<47> as *const (),
    missing::<48> as *const (),
    purchase_ui_async as *const (),
    missing::<50> as *const (),
    missing::<51> as *const (),
    missing::<52> as *const (),
    redeem_ui_async as *const (),
    missing::<54> as *const (),
    updates_async as *const (),
    count_status as *const (),
    array_status as *const (),
    missing::<58> as *const (),
    missing::<59> as *const (),
    install_async as *const (),
    status as *const (),
    install_async as *const (),
    count_status as *const (),
    array_status as *const (),
    package_identifier as *const (),
    register as *const (),
    unregister as *const (),
    register as *const (),
    unregister as *const (),
    missing::<70> as *const (),
    license_for_durables_async as *const (),
    license_result as *const (),
    missing::<73> as *const (),
    missing::<74> as *const (),
    product_page_ui_async as *const (),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn the_game_is_licensed_and_the_store_is_offline() {
        let this: This = &OBJECT;
        assert_eq!(size_of::<GameLicense>(), 104);
        type Begin = extern "system" fn(This, Handle, *mut AsyncBlock) -> HRESULT;
        type Result = extern "system" fn(This, *mut AsyncBlock, *mut GameLicense) -> HRESULT;
        let mut b = block();
        assert_eq!(
            method::<Begin>(&OBJECT, 0xe0)(this, handle(&CONTEXT), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut license = std::mem::MaybeUninit::<GameLicense>::zeroed();
        assert_eq!(
            method::<Result>(&OBJECT, 0xe8)(this, &mut b, license.as_mut_ptr()),
            S_OK
        );
        // SAFETY: written by the call.
        let license = unsafe { license.assume_init() };
        assert!(license.is_active && !license.is_trial);

        let mut b = block();
        assert_eq!(
            method::<Begin>(&OBJECT, 0x1b8)(this, handle(&CONTEXT), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
        type Count = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;
        assert_eq!(
            method::<Count>(&OBJECT, 0x1c0)(this, &mut b, &mut 0),
            NETWORK_ERROR
        );

        type Durable = extern "system" fn(This, Handle, *const c_char, *mut AsyncBlock) -> HRESULT;
        type License = extern "system" fn(This, *mut AsyncBlock, *mut Handle) -> HRESULT;
        let mut b = block();
        let durable = method::<Durable>(&OBJECT, 0x238);
        assert_eq!(
            durable(this, handle(&CONTEXT), c"9XYZ".as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut l = std::ptr::null();
        assert_eq!(
            method::<License>(&OBJECT, 0x240)(this, &mut b, &mut l),
            S_OK
        );
        assert!(!method::<extern "system" fn(This, Handle) -> bool>(
            &OBJECT, 0xb0
        )(this, l));
    }
}
