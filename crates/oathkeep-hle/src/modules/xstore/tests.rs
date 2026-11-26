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
fn offline_async_calls_report_network_error_from_their_result_slots() {
    assert!(VTABLE.0.len() > 0x260 / 8);

    let this: This = &OBJECT;
    type StoreIdAsync =
        extern "system" fn(This, Handle, *const c_char, *mut AsyncBlock) -> HRESULT;
    type ReportFulfillmentAsync = extern "system" fn(
        This,
        Handle,
        *const c_char,
        u32,
        *const c_void,
        *mut AsyncBlock,
    ) -> HRESULT;
    type PurchaseUiAsync = extern "system" fn(
        This,
        Handle,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut AsyncBlock,
    ) -> HRESULT;
    type RedeemUiAsync = extern "system" fn(
        This,
        Handle,
        *const c_char,
        *const *const c_char,
        usize,
        bool,
        *mut AsyncBlock,
    ) -> HRESULT;
    type StatusResult = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
    type CountResult = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;

    let mut b = block();
    assert_eq!(
        method::<StoreIdAsync>(&OBJECT, 0x258)(
            this,
            handle(&CONTEXT),
            c"9DUMMY".as_ptr(),
            &mut b,
        ),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
    assert_eq!(
        method::<StatusResult>(&OBJECT, 0x260)(this, &mut b),
        NETWORK_ERROR
    );

    let mut b = block();
    let mut remaining = 7u32;
    assert_eq!(
        method::<ReportFulfillmentAsync>(&OBJECT, 0x118)(
            this,
            handle(&CONTEXT),
            c"9DUMMY".as_ptr(),
            1,
            std::ptr::null(),
            &mut b,
        ),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
    assert_eq!(
        method::<CountResult>(&OBJECT, 0x120)(this, &mut b, &mut remaining),
        NETWORK_ERROR
    );
    assert_eq!(remaining, 7);

    let mut b = block();
    assert_eq!(
        method::<PurchaseUiAsync>(&OBJECT, 0x188)(
            this,
            handle(&CONTEXT),
            c"9DUMMY".as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut b,
        ),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
    assert_eq!(
        method::<StatusResult>(&OBJECT, 0x190)(this, &mut b),
        NETWORK_ERROR
    );

    let mut b = block();
    assert_eq!(
        method::<RedeemUiAsync>(&OBJECT, 0x1a8)(
            this,
            handle(&CONTEXT),
            c"DUMMY-TOKEN".as_ptr(),
            std::ptr::null(),
            0,
            false,
            &mut b,
        ),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
    assert_eq!(
        method::<StatusResult>(&OBJECT, 0x1b0)(this, &mut b),
        NETWORK_ERROR
    );

    let mut b = block();
    let mut balance = 3u32;
    assert_eq!(
        method::<StoreIdAsync>(&OBJECT, 0x108)(
            this,
            handle(&CONTEXT),
            c"9DUMMY".as_ptr(),
            &mut b,
        ),
        S_OK
    );
    assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NETWORK_ERROR);
    assert_eq!(
        method::<CountResult>(&OBJECT, 0x110)(this, &mut b, &mut balance),
        NETWORK_ERROR
    );
    assert_eq!(balance, 3);
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

#[test]
fn a_store_id_without_a_package_is_an_invalid_argument() {
    type Query = extern "system" fn(This, *const c_char, usize, *mut u8) -> HRESULT;
    let query = method::<Query>(&OBJECT, 0x208);
    let mut id = [0u8; 33];
    assert_eq!(
        query(&OBJECT, c"9NOTINSTALLED".as_ptr(), 33, id.as_mut_ptr()),
        E_INVALIDARG
    );
}
