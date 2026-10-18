use std::{ffi::c_void, ptr};

use windows::{
    core::{IUnknown, Interface, GUID, HRESULT, PCWSTR},
    Win32::{
        Media::Audio::{eCommunications, eConsole, eMultimedia, ERole},
        System::Com::{CoCreateInstance, CLSCTX_ALL},
    },
};

use crate::error::{BackendError, Result};

// IPolicyConfig is an undocumented but stable Windows audio policy COM
// interface. It is the API used by the Sound control panel to assign the
// default endpoint for each ERole.
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_values(
    0x870af99c,
    0x171d,
    0x4f9e,
    [0xaf, 0x0d, 0xe6, 0x3d, 0xf4, 0x0c, 0x2b, 0xc9],
);
const IID_POLICY_CONFIG: GUID = GUID::from_values(
    0xf8679f50,
    0x850a,
    0x41cf,
    [0x9c, 0x72, 0x43, 0x0f, 0x29, 0x02, 0x90, 0xc8],
);
const SET_DEFAULT_ENDPOINT_VTABLE_INDEX: usize = 13;

type QueryInterface =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type Release = unsafe extern "system" fn(*mut c_void) -> u32;
type SetDefaultEndpoint = unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT;

fn hresult_error(operation: &str, result: HRESULT) -> BackendError {
    BackendError::Audio(format!(
        "{operation}に失敗しました (HRESULT=0x{:08X})",
        result.0 as u32
    ))
}

pub fn set_default_endpoint(endpoint_id: &str, communications: bool) -> Result<()> {
    let mut endpoint_id_wide: Vec<u16> = endpoint_id.encode_utf16().collect();
    endpoint_id_wide.push(0);

    let policy: IUnknown = unsafe {
        CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL).map_err(|error| {
            BackendError::Audio(format!(
                "Windows既定デバイスAPIの作成に失敗しました: {error}"
            ))
        })?
    };

    let policy_raw = policy.as_raw() as *mut c_void;
    let unknown_vtable = unsafe { *(policy_raw as *const *const *const c_void) };
    let query_interface: QueryInterface = unsafe { std::mem::transmute(*unknown_vtable.add(0)) };
    let mut policy_config = ptr::null_mut();
    let query_result = unsafe {
        query_interface(
            policy_raw,
            &IID_POLICY_CONFIG,
            &mut policy_config as *mut *mut c_void,
        )
    };
    if query_result.0 < 0 {
        return Err(hresult_error(
            "Windows既定デバイスインターフェースの取得",
            query_result,
        ));
    }

    let policy_vtable = unsafe { *(policy_config as *const *const *const c_void) };
    // IPolicyConfig has ten methods between IUnknown and SetDefaultEndpoint.
    // The three IUnknown entries are followed by methods 0..9, so
    // SetDefaultEndpoint is vtable slot 13 (not slot 12, which is
    // SetPropertyValue and has a different ABI).
    let set_default: SetDefaultEndpoint =
        unsafe { std::mem::transmute(*policy_vtable.add(SET_DEFAULT_ENDPOINT_VTABLE_INDEX)) };
    let roles: &[ERole] = if communications {
        &[eCommunications]
    } else {
        &[eConsole, eMultimedia]
    };
    let mut operation_result = Ok(());
    for role in roles {
        let result =
            unsafe { set_default(policy_config, PCWSTR(endpoint_id_wide.as_ptr()), *role) };
        if result.0 < 0 {
            operation_result = Err(hresult_error("Windows既定デバイスの設定", result));
            break;
        }
    }

    let release: Release = unsafe { std::mem::transmute(*policy_vtable.add(2)) };
    unsafe { release(policy_config) };
    operation_result
}
