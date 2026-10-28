//! WinINET 的连接级设置与通知。直接改 ProxyEnable 不会同步连接配置/进程缓存。
use std::mem::size_of;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Networking::WinInet::*;

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionSettings {
    pub flags: u32,
    pub server: String,
    pub bypass: String,
    pub auto_config_url: String,
}

impl ConnectionSettings {
    pub fn for_port(port: u16) -> Self {
        Self {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: super::sysproxy_set::proxy_server_value(port),
            // 公网域名不能继承旧 PAC / bypass 的 DIRECT 例外；私网分流交给 mihomo。
            bypass: "localhost;127.*;[::1]".into(),
            auto_config_url: String::new(),
        }
    }

    pub fn manual_server(&self) -> Option<&str> {
        (self.flags & PROXY_TYPE_PROXY != 0 && !self.server.trim().is_empty())
            .then_some(self.server.as_str())
    }
}

fn list(options: &mut [INTERNET_PER_CONN_OPTIONW]) -> INTERNET_PER_CONN_OPTION_LISTW {
    INTERNET_PER_CONN_OPTION_LISTW {
        dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
        pszConnection: null_mut(), // 默认 LAN 连接
        dwOptionCount: options.len() as u32,
        dwOptionError: 0,
        pOptions: options.as_mut_ptr(),
    }
}

pub fn read() -> Result<ConnectionSettings, String> {
    let mut options = [
        // Win7+ 必须用 FLAGS_UI 查询配置；FLAGS 可能仍是旧连接的运行状态。
        // 写入/还原仍用 FLAGS（Microsoft INTERNET_PER_CONN_OPTION 文档）。
        INTERNET_PER_CONN_FLAGS_UI,
        INTERNET_PER_CONN_PROXY_SERVER,
        INTERNET_PER_CONN_PROXY_BYPASS,
        INTERNET_PER_CONN_AUTOCONFIG_URL,
    ]
    .map(|dw_option| INTERNET_PER_CONN_OPTIONW {
        dwOption: dw_option,
        Value: INTERNET_PER_CONN_OPTIONW_0 {
            pszValue: null_mut(),
        },
    });
    let mut list = list(&mut options);
    let mut size = list.dwSize;
    // SAFETY: list/options 可写且活到调用结束；返回的字符串由 WinINET 分配。
    let ok = unsafe {
        InternetQueryOptionW(
            null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            &mut list as *mut _ as *mut _,
            &mut size,
        )
    };
    let error = (ok == 0).then(std::io::Error::last_os_error);
    let mut strings = Vec::new();
    for option in &options[1..] {
        // SAFETY: 这些选项的 union 成员为 NUL 结尾宽字符串；零初始化允许失败清理。
        unsafe {
            let ptr = option.Value.pszValue;
            let value = if ptr.is_null() {
                String::new()
            } else {
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let value = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                GlobalFree(ptr as _);
                value
            };
            strings.push(value);
        }
    }
    if let Some(error) = error {
        return Err(format!("读取 Windows 代理连接配置失败: {error}"));
    }
    Ok(ConnectionSettings {
        // SAFETY: FLAGS 选项返回 DWORD。
        flags: unsafe { options[0].Value.dwValue },
        server: strings.remove(0),
        bypass: strings.remove(0),
        auto_config_url: strings.remove(0),
    })
}

pub fn write(settings: &ConnectionSettings) -> Result<(), String> {
    let mut strings: Vec<Vec<u16>> = [
        &settings.server,
        &settings.bypass,
        &settings.auto_config_url,
    ]
    .iter()
    .map(|s| s.encode_utf16().chain(Some(0)).collect())
    .collect();
    let mut options = vec![INTERNET_PER_CONN_OPTIONW {
        dwOption: INTERNET_PER_CONN_FLAGS,
        Value: INTERNET_PER_CONN_OPTIONW_0 {
            dwValue: settings.flags,
        },
    }];
    for (dw_option, value) in [
        INTERNET_PER_CONN_PROXY_SERVER,
        INTERNET_PER_CONN_PROXY_BYPASS,
        INTERNET_PER_CONN_AUTOCONFIG_URL,
    ]
    .into_iter()
    .zip(&mut strings)
    {
        options.push(INTERNET_PER_CONN_OPTIONW {
            dwOption: dw_option,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                pszValue: value.as_mut_ptr(),
            },
        });
    }
    let list = list(&mut options);
    // SAFETY: list/options/strings 均在同步调用期间有效。
    if unsafe {
        InternetSetOptionW(
            null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            &list as *const _ as *const _,
            list.dwSize,
        )
    } == 0
    {
        return Err(format!(
            "设置 Windows 代理连接配置失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    // 环境变量的 WM_SETTINGCHANGE 不能代替 WinINET 的变更通知与刷新。
    for option in [INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_OPTION_REFRESH] {
        // SAFETY: 全局通知选项不需要缓冲区。
        if unsafe { InternetSetOptionW(null(), option, null(), 0) } == 0 {
            return Err(format!(
                "刷新 Windows 代理设置失败: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}
