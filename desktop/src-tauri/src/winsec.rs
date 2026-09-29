//! Windows 安全原语(dev-fixture 凭据面,F3-c3):密钥文件的 owner-only
//! DACL(0600 等价物)。
//!
//! Unix 夹具以 0600 权限位收窄密钥文件(属主读写,无 group/world 访问面);
//! Windows 等价物是在创建时一次到位的显式 DACL:恰好一条 allow-ACE,受权方
//! 为当前进程 token 的 user SID(与 `nlos_ipc::windows::SameUserAuthorizer`
//! 的准入参照同源),掩码 `FILE_ALL_ACCESS`。之所以经 `CreateFileW` 的
//! `SECURITY_ATTRIBUTES` 一次到位、而不是"先按默认 DACL 创建再用
//! SetSecurityInfo 收窄",是因为后者的默认 DACL 存续窗口里文件完整继承
//! 用户 temp 目录的 ACE 面(`key_file_acl_side.rs` 的红态即此形态)。

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, NO_INHERITANCE, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorDacl,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

/// 以 owner-only DACL 创建新文件(已存在则失败,语义同 `create_new`);
/// 返回的 `File` 由调用方写入并关闭。
///
/// # Errors
///
/// 当前用户 SID 捕获、ACL/安全描述符构造或 `CreateFileW` 任一失败都返回
/// 类型化 `io::Error`(fail-closed:失败路径上不存在半创建文件——
/// `CREATE_NEW` 失败即无文件落盘)。
pub fn create_new_owner_only(path: &Path) -> io::Result<File> {
    let owner_sid = nlos_ipc::windows::current_process_user_sid()?;
    // 单条 allow-ACE:当前用户 FILE_ALL_ACCESS、无继承面。
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: owner_sid.as_ptr().cast::<u16>().cast_mut(),
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: trustee,
    };
    let mut acl = std::ptr::null_mut();
    // SAFETY: entry 为按值传入的显式访问项,oldacl 为 null(全新 ACL),
    // acl out-pointer 有效;返回的 ACL 由 LocalFree 释放。
    let built = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &raw mut acl) };
    if built != 0 || acl.is_null() {
        return Err(io::Error::from_raw_os_error(built as i32));
    }

    // 绝对形安全描述符:无 owner/group 字段,保护性 DACL 指向上面的 ACL。
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    // SAFETY: descriptor 为足容栈对象(stack 绝对形 SD 的标准用法)。
    if unsafe {
        InitializeSecurityDescriptor((&raw mut descriptor).cast(), SECURITY_DESCRIPTOR_REVISION)
    } == 0
    {
        // SAFETY: acl 来自成功的 SetEntriesInAclW,恰好释放一次。
        unsafe { LocalFree(acl.cast()) };
        return Err(io::Error::last_os_error());
    }
    // SAFETY: descriptor 已初始化;acl 为非 null 的有效 ACL;非默认 DACL。
    if unsafe { SetSecurityDescriptorDacl((&raw mut descriptor).cast(), 1, acl, 0) } == 0 {
        // SAFETY: 同上,恰好释放一次。
        unsafe { LocalFree(acl.cast()) };
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&raw mut descriptor).cast(),
        bInheritHandle: 0,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: wide 为 NUL 结尾的 UTF-16 路径;attributes 指向本函数栈上的
    // 合法 SD(绝对形,调用期间存活);共享模式 0(独占)。
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: acl 在 CreateFileW 消费完 SD 后释放,恰好一次。
    unsafe { LocalFree(acl.cast()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: handle 为 CreateFileW 返回的独占拥有句柄,移交 File 独占所有权。
    Ok(unsafe { File::from_raw_handle(handle) })
}
