//! F3-c3 Windows 凭据面集成证据(dev-fixture):密钥文件的 owner-only
//! DACL(0600 等价物)。经夹具公开入口 [`DevFixture::write_key_file`] 黑盒
//! 落盘后,读回文件 DACL 并断言:
//! (1) 恰好 1 条 ACE(无继承面——默认继承用户 temp 目录 ACL 的形态是
//!     多 ACE/继承位,即本测试的红态);
//! (2) ACE 为 allow 型、非继承、掩码 = FILE_ALL_ACCESS(0600 的"属主
//!     完全控制"等价物);
//! (3) 受权 SID 与当前进程 token 的 user SID 逐字节相等(nlos-ipc 凭据
//!     面的同一 SID 源)。

#![cfg(all(windows, feature = "dev-fixture"))]

use std::fs;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;

use llmos_desktop_lib::devfixture::DevFixture;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, INHERITED_ACE,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

#[tokio::test]
async fn key_file_dacl_grants_full_access_to_the_current_user_only() {
    let fixture = DevFixture::spawn("c1").expect("fixture");
    let path = std::env::temp_dir().join(format!("llmosdt-acl-{}.key", std::process::id()));
    let _ = fs::remove_file(&path);
    fixture.write_key_file(&path).expect("write key file");

    // GENERIC_READ 隐含 READ_CONTROL,足够 GetSecurityInfo 读取 DACL。
    let file = fs::File::open(&path).expect("reopen key file");
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: `file` 的句柄存活于本作用域;三个 out-pointer 均有效;返回的
    // 安全描述符由 LocalFree 释放,DACL 指针归属于它。
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &raw mut dacl,
            null_mut(),
            &raw mut descriptor,
        )
    };
    assert_eq!(
        status, 0,
        "GetSecurityInfo failed with WIN32_ERROR {status}"
    );

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl 来自成功的 GetSecurityInfo;size 为足容 out 结构。
    let counted = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    assert_ne!(counted, 0, "GetAclInformation failed");
    assert_eq!(
        size.AceCount, 1,
        "owner-only DACL must carry exactly one ACE (found {})",
        size.AceCount
    );

    let mut ace = null_mut();
    // SAFETY: ACE 索引 0 存在(AceCount == 1);ace out-pointer 有效;ACE
    // 内存归属于 descriptor,在 LocalFree 前完成读取。
    let fetched = unsafe { GetAce(dacl, 0, &raw mut ace) };
    assert_ne!(fetched, 0, "GetAce failed");
    // SAFETY: GetAce 成功后指向 ACCESS_ALLOWED_ACE(类型先断言再读字段)。
    let ace = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    assert_eq!(u32::from(ace.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
    assert_eq!(
        u32::from(ace.Header.AceFlags & u8::try_from(INHERITED_ACE).unwrap_or(0)),
        0,
        "the owner ACE must be explicit, not inherited from the temp directory"
    );
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);

    let owner_sid = nlos_ipc::windows::current_process_user_sid().expect("current user sid");
    // SAFETY: SidStart 起始于 ACCESS_ALLOWED_ACE 内偏移 4 处,是受权 SID;
    // owner_sid 为 GetLengthSid 校验过的完整 SID 字节。
    let same = unsafe {
        EqualSid(
            (&raw const ace.SidStart)
                .cast::<core::ffi::c_void>()
                .cast_mut(),
            owner_sid.as_ptr().cast::<core::ffi::c_void>().cast_mut(),
        )
    };
    assert_ne!(same, 0, "the sole ACE must grant the current user");

    // SAFETY: descriptor 由 GetSecurityInfo 分配,LocalFree 恰好释放一次。
    unsafe { LocalFree(descriptor.cast()) };
    drop(file);
    let _ = fs::remove_file(&path);
}
