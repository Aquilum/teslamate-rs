//! Linux PAM password checks for the web UI.
//!
//! Authenticate only — do not open a PAM session (no utmp / env dance).

pub fn service_name() -> String {
    std::env::var("TESLAMATE_RS_PAM_SERVICE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "teslamate-rs".into())
}

pub fn required_group() -> Option<String> {
    match std::env::var("TESLAMATE_RS_PAM_GROUP") {
        Ok(s) if s.is_empty() || s == "none" || s == "-" => None,
        Ok(s) => Some(s),
        Err(_) => Some("teslamate-rs".into()),
    }
}

pub fn allow_root() -> bool {
    matches!(
        std::env::var("TESLAMATE_RS_PAM_ALLOW_ROOT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::service_name;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};
    use std::ptr;
    use std::sync::Mutex;

    const PAM_SUCCESS: c_int = 0;
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_ERROR_MSG: c_int = 3;
    const PAM_TEXT_INFO: c_int = 4;
    const PAM_CONV_ERR: c_int = 19;

    static NSS: Mutex<()> = Mutex::new(());

    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    #[repr(C)]
    struct PamConv {
        conv: Option<
            unsafe extern "C" fn(
                num_msg: c_int,
                msg: *mut *const PamMessage,
                resp: *mut *mut PamResponse,
                appdata_ptr: *mut c_void,
            ) -> c_int,
        >,
        appdata_ptr: *mut c_void,
    }

    enum PamHandle {}

    #[link(name = "pam")]
    unsafe extern "C" {
        fn pam_start(
            service_name: *const c_char,
            user: *const c_char,
            pam_conversation: *const PamConv,
            pamh: *mut *mut PamHandle,
        ) -> c_int;
        fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
        fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
        fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;
    }

    struct Creds {
        user: CString,
        pass: CString,
    }

    unsafe fn free_replies(replies: *mut PamResponse, n: usize) {
        if replies.is_null() {
            return;
        }
        for i in 0..n {
            let slot = replies.add(i);
            if !(*slot).resp.is_null() {
                libc::free((*slot).resp as *mut c_void);
            }
        }
        libc::free(replies as *mut c_void);
    }

    unsafe extern "C" fn conversation(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int {
        if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
            return PAM_CONV_ERR;
        }
        let creds = &*(appdata_ptr as *const Creds);
        let n = num_msg as usize;
        let replies = libc::calloc(n, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if replies.is_null() {
            return PAM_CONV_ERR;
        }
        for i in 0..n {
            let message = *msg.add(i);
            if message.is_null() {
                free_replies(replies, i);
                return PAM_CONV_ERR;
            }
            let answer = match (*message).msg_style {
                PAM_PROMPT_ECHO_OFF => creds.pass.as_ptr(),
                PAM_PROMPT_ECHO_ON => creds.user.as_ptr(),
                PAM_ERROR_MSG | PAM_TEXT_INFO => ptr::null(),
                _ => {
                    free_replies(replies, i);
                    return PAM_CONV_ERR;
                }
            };
            let slot = replies.add(i);
            (*slot).resp_retcode = 0;
            if answer.is_null() {
                (*slot).resp = ptr::null_mut();
            } else {
                (*slot).resp = libc::strdup(answer);
                if (*slot).resp.is_null() {
                    free_replies(replies, i);
                    return PAM_CONV_ERR;
                }
            }
        }
        *resp = replies;
        PAM_SUCCESS
    }

    pub fn authenticate(username: &str, password: &str) -> Result<(), String> {
        let service = CString::new(service_name()).map_err(|_| "invalid PAM service")?;
        let creds = Creds {
            user: CString::new(username).map_err(|_| "invalid username")?,
            pass: CString::new(password).map_err(|_| "invalid password")?,
        };
        let conv = PamConv {
            conv: Some(conversation),
            appdata_ptr: &creds as *const Creds as *mut c_void,
        };
        let mut pamh: *mut PamHandle = ptr::null_mut();
        unsafe {
            let mut status = pam_start(service.as_ptr(), creds.user.as_ptr(), &conv, &mut pamh);
            if status != PAM_SUCCESS {
                if !pamh.is_null() {
                    pam_end(pamh, status);
                }
                return Err("PAM is not available".into());
            }
            status = pam_authenticate(pamh, 0);
            if status == PAM_SUCCESS {
                status = pam_acct_mgmt(pamh, 0);
            }
            pam_end(pamh, status);
            if status == PAM_SUCCESS {
                Ok(())
            } else {
                Err("invalid username or password".into())
            }
        }
    }

    pub fn group_exists(group: &str) -> bool {
        let Ok(c) = CString::new(group) else {
            return false;
        };
        let _lock = NSS.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { !libc::getgrnam(c.as_ptr()).is_null() }
    }

    pub fn user_in_group(username: &str, group: &str) -> Result<bool, String> {
        let user_c = CString::new(username).map_err(|_| "invalid username")?;
        let group_c = CString::new(group).map_err(|_| "invalid group")?;
        let _lock = NSS.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let pwd = libc::getpwnam(user_c.as_ptr());
            if pwd.is_null() {
                return Ok(false);
            }
            let grp = libc::getgrnam(group_c.as_ptr());
            if grp.is_null() {
                return Ok(false);
            }
            let primary = (*pwd).pw_gid;
            let want = (*grp).gr_gid;
            if primary == want {
                return Ok(true);
            }
            let mut n = 32i32;
            let mut groups = vec![0 as libc::gid_t; n as usize];
            let mut rc = libc::getgrouplist(user_c.as_ptr(), primary, groups.as_mut_ptr(), &mut n);
            if rc < 0 {
                groups.resize(n.max(0) as usize, 0);
                rc = libc::getgrouplist(user_c.as_ptr(), primary, groups.as_mut_ptr(), &mut n);
                if rc < 0 {
                    return Err("could not read group membership".into());
                }
            }
            Ok(groups.iter().take(n.max(0) as usize).any(|gid| *gid == want))
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{authenticate, group_exists, user_in_group};

#[cfg(not(target_os = "linux"))]
pub fn authenticate(_username: &str, _password: &str) -> Result<(), String> {
    Err("PAM is only available on Linux".into())
}

#[cfg(not(target_os = "linux"))]
pub fn group_exists(_group: &str) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub fn user_in_group(_username: &str, _group: &str) -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_group_is_teslamate_rs() {
        std::env::remove_var("TESLAMATE_RS_PAM_GROUP");
        assert_eq!(required_group().as_deref(), Some("teslamate-rs"));
    }

    #[test]
    fn none_disables_group() {
        let prev = std::env::var("TESLAMATE_RS_PAM_GROUP").ok();
        std::env::set_var("TESLAMATE_RS_PAM_GROUP", "none");
        assert_eq!(required_group(), None);
        match prev {
            Some(v) => std::env::set_var("TESLAMATE_RS_PAM_GROUP", v),
            None => std::env::remove_var("TESLAMATE_RS_PAM_GROUP"),
        }
    }
}
