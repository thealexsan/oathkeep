use std::ffi::c_void;

use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::alloc::BasicInformation;
use super::physical::page_table;
use super::physical_io::View;
use super::{XMemAllocatePhysicalPages, XMemVirtualQuery};

mod alloc;
mod mapping;
mod physical_io;

fn query(address: *const c_void) -> BasicInformation {
    // SAFETY: plain data; zero is a valid value.
    let mut info: BasicInformation = unsafe { std::mem::zeroed() };
    // SAFETY: a pseudo handle needs no closing.
    let process = unsafe { GetCurrentProcess() };
    assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
    info
}

/// `n` pages of one new allocation.
fn allocate(n: usize) -> Vec<usize> {
    let (mut pages, mut given) = (vec![0; n], n);
    assert_eq!(
        XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
        1
    );
    assert_eq!(given, n);
    pages
}

/// The bytes of one page, through a view of our own.
fn page_bytes(page: usize) -> Vec<u8> {
    let slice = page_table().pages.get(&page).cloned().expect("known page");
    View::map(&slice, 1).expect("mapped").bytes().to_vec()
}
