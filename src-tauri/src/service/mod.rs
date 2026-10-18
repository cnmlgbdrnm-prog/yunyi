//! 提权服务：以 SYSTEM 身份 spawn/kill mihomo，供 TUN 全局模式使用。
//!
//! 只有 mihomo 的进程生命周期在服务侧；端口选择、配置生成、就绪探测、系统代理
//! （HKCU，见 net/sysproxy_set.rs）与 local_proxy 登记全部留在主程序——SYSTEM
//! 服务写的是它自己的用户 hive，写不到用户的 WinINET 设置。
//!
//! 服务与应用是**同一个 exe**：SCM 注册的命令行是 `<exe> --service`，由 main.rs
//! 的入口分流接住。这样服务能直接复用 config 等现有模块（项目是单二进制、无
//! lib.rs，另开 bin 复用不了），且「调用方必须是同一个 exe 路径」这条身份校验
//! 天然成立。

pub mod client;
pub mod config_guard;
pub mod http;
pub mod paths;
#[cfg(windows)]
pub mod process;
pub mod protocol;
/// 服务自有目录的 DACL 加固只在 Windows 上有意义（也没有跨平台 API 可用）
#[cfg(windows)]
pub mod secure_dir;
#[cfg(windows)]
pub mod session;

/// 服务宿主与命名管道只在 Windows 编译：SCM 派发与管道都是 Win32 API，
/// 非 Windows 上留着只会引入编译不了的分支（照 net/sysproxy_set.rs 的既有写法）。
#[cfg(windows)]
pub mod host;
#[cfg(windows)]
pub mod install;
pub mod mihomo;
#[cfg(windows)]
pub mod pipe;

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/service/mods.rs"
));
