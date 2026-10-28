# 非 TUN 模式系统代理接管修复

## 已确认的现象

- 本机排查时 Windows `ProxyEnable=0`、`ProxyServer` 为空，而 mihomo 的 7890 端口仍在监听。
- 控制器回读：`mode=rule`、`mixed-port=7890`、`tun.enable=true`。当前会话已开启 TUN；本次没有关闭它。
- 原实现直接写代理注册表，仅广播环境变量变更，没有调用 WinINET 的代理设置通知和刷新。
- 运行期间没有独立的代理维护任务；关闭 TUN 前没有重新验证系统代理，界面查询未知时还可能显示“系统代理已接入”。

上述证据证明接管链路存在缺口，但不能确定最初是哪一个进程关闭了系统代理。

## 修改

1. 同步旧式代理注册表，并用 `InternetSetOptionW(INTERNET_OPTION_PER_CONNECTION_OPTION)` 设置默认 LAN 连接代理，然后发送 `SETTINGS_CHANGED` 和 `REFRESH`，同时回读 WinINET、注册表和用户环境变量。按 Windows 7+ 官方接口要求，查询使用 `FLAGS_UI`，写入/还原使用 `FLAGS`，避免将旧连接状态误当成当前配置。
2. 开启期间清除旧 PAC/自动探测及公网绕过规则的影响；快照保存原有 flags、server、bypass、PAC URL、环境变量，关闭时恢复。写入失败尝试完整回滚，回滚错误也会返回。
3. 每个网络会话启动一个后台维护任务，每 2 秒检查一次；TUN 关闭、代理进程存活且控制器就绪时，恢复丢失的系统代理。检测到其它手动代理时提示冲突，不循环抢写。
4. 维护任务持有网络会话锁，核对会话 token；停止或新会话替换后旧任务退出，恢复时不覆盖最初快照。
5. 关闭 TUN 前先恢复并确认系统代理；失败则不撤隧道。启动代理配置失败时收掉已启动的 mihomo。
6. 查询未知时明确显示未知，TUN 开启时显示 TUN 状态，不再误称系统代理已接入。

## 验证

- `cargo check --offline --manifest-path src-tauri/Cargo.toml`：通过。
- `cargo test --offline --manifest-path src-tauri/Cargo.toml --features local-tests proxy_recovery -- --include-ignored --test-threads=1`：7 项专项测试通过。
- 实机专项包含：原生关闭代理、直接将注册表 `ProxyEnable` 改为 0、恢复后回读、后台无界面自动恢复、原快照不被覆盖、清除会话后不再写入、完整还原原连接设置。
- 现有 sysproxy 相关回归：19 项通过。跳过 3 个会将系统代理切向非监听测试端口的旧实机用例，使用上述真实 7890 往返用例代替。
- NetworkCard 与 Dashboard：33 项前端测试通过。
- 产品与测试 TypeScript 检查、Vite 构建：通过。

## 生效与范围

修复版启动路径：`D:\新建文件夹\vpn控制\src-tauri\target\debug\vpn-ctl.exe`。

旧运行文件备份：`D:\新建文件夹\vpn控制\src-tauri\target\debug\vpn-ctl.exe.before-proxy-recovery-20261028`。

正在运行的旧进程不会因磁盘文件替换而获得新逻辑，需要重启云驿。测试结束已还原当时系统代理设置，未操作远程服务器。

本修复不把普通系统代理变成全局透明代理。不读取系统代理的程序仍需要自身代理配置、TUN 或程序级防火墙阻断；2 秒后台检查也不是零时间窗口的防泄漏保证。现有防火墙规则范围与策略未修改。验证覆盖 Windows 配置与恢复链路，未逐个验证所有第三方软件，也未执行整套应用 E2E。

## 接口依据

- [Microsoft：WinINET Option Flags](https://learn.microsoft.com/en-us/windows/win32/wininet/option-flags)
- [Microsoft：Setting and Retrieving Internet Options](https://learn.microsoft.com/en-us/windows/win32/wininet/setting-and-retrieving-internet-options)
- [Microsoft：INTERNET_PER_CONN_OPTION 与 FLAGS_UI](https://learn.microsoft.com/en-us/windows/win32/api/wininet/ns-wininet-internet_per_conn_optiona)
- [Chromium：Proxy settings and fallback](https://www.chromium.org/developers/design-documents/network-stack/proxy-settings-fallback/)
