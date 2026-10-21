//! Tauri IPC 壳：每个 `*_cmd` 只提取 `State` 并转调本模块业务函数。
//!
//! 从 `commands.rs` 拆出以控制单文件规模（≤1500 行）；根模块 `pub use ipc::*;`，
//! 因此命令路径、`generate_handler!` 注册与前端 invoke 名称全部不变。

use super::*;

#[tauri::command]
pub async fn list_subscriptions_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<Vec<Subscription>> {
    list_subscriptions(state.inner().clone())
}

#[tauri::command]
pub async fn create_subscription_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
    url: String,
    enabled: bool,
    filter_json: String,
) -> R<Subscription> {
    create_subscription(state.inner().clone(), name, url, enabled, filter_json)
}

#[tauri::command]
pub async fn update_subscription_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
    name: String,
    url: String,
    enabled: bool,
    filter_json: String,
) -> R<Subscription> {
    update_subscription(state.inner().clone(), id, name, url, enabled, filter_json).await
}

#[tauri::command]
pub async fn delete_subscription_cmd(state: tauri::State<'_, Arc<AppState>>, id: String) -> R<()> {
    delete_subscription(state.inner().clone(), id).await
}

#[tauri::command]
pub async fn sync_subscription_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
) -> R<SyncOutcome> {
    sync_subscription(state.inner().clone(), id).await
}

#[tauri::command]
pub async fn list_nodes_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<Node>> {
    list_nodes(state.inner().clone())
}

#[tauri::command]
pub async fn set_node_enabled_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
    enabled: bool,
) -> R<()> {
    set_node_enabled(state.inner().clone(), id, enabled).await
}

#[tauri::command]
pub async fn toggle_node_relay_cmd(state: tauri::State<'_, Arc<AppState>>, id: String) -> R<()> {
    toggle_node_relay(state.inner().clone(), id).await
}

/// 仅更换 REALITY 伪装域名（不重部署）：服务器 xray config 的 dest/serverNames
/// 就地改写 + 重启验证 + 回写节点 sni；公钥/uuid/端口不动。
pub async fn change_reality_dest(state: Arc<AppState>, server_id: i64, dest: String) -> R<()> {
    crate::servers::reality_dest::change_reality_dest(&state, server_id, &dest)
        .await
        .map_err(err)?;
    // 节点参数变了（sni），必须先让运行中的客户端跟上再返回：服务器那边已经要求新
    // SNI，客户端还发旧的 = 那条节点当场连不上，而界面上一切正常
    rebuild_and_apply(&state).await
}

#[tauri::command]
pub async fn change_reality_dest_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    dest: String,
) -> R<()> {
    change_reality_dest(state.inner().clone(), server_id, dest).await
}

#[tauri::command]
pub async fn delete_node_cmd(state: tauri::State<'_, Arc<AppState>>, id: String) -> R<()> {
    delete_node(state.inner().clone(), id).await
}

#[tauri::command]
pub async fn rename_node_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
    name: String,
) -> R<()> {
    rename_node(state.inner().clone(), id, name).await
}

#[tauri::command]
pub async fn get_merged_config_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<String> {
    get_merged_config(state.inner().clone()).await
}

#[tauri::command]
pub async fn export_yaml_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Option<ExportOutcome>> {
    export_yaml(state.inner().clone(), || {
        native_save_dialog("YAML", &["yaml", "yml"], "vpn-ctl.yaml")
    })
    .await
}

#[tauri::command]
pub async fn export_singbox_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<Option<ExportOutcome>> {
    export_singbox(state.inner().clone(), || {
        native_save_dialog("JSON", &["json"], "yunyi-singbox.json")
    })
    .await
}

#[tauri::command]
pub fn list_exportable_nodes_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<Vec<crate::domain::ExportableNode>> {
    list_exportable_nodes(state.inner().clone())
}

#[tauri::command]
pub async fn export_yaml_selected_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    node_ids: Vec<String>,
) -> R<Option<ExportOutcome>> {
    export_yaml_selected(state.inner().clone(), node_ids, || {
        native_save_dialog("YAML", &["yaml", "yml"], "vpn-ctl.yaml")
    })
    .await
}

#[tauri::command]
pub async fn export_singbox_selected_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    node_ids: Vec<String>,
) -> R<Option<ExportOutcome>> {
    export_singbox_selected(state.inner().clone(), node_ids, || {
        native_save_dialog("JSON", &["json"], "yunyi-singbox.json")
    })
    .await
}

// ---------- 分享 ----------

#[tauri::command]
pub async fn list_share_candidates_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<crate::share::ShareCandidate>> {
    crate::share::share_candidates(state.inner(), server_id).map_err(err)
}

#[tauri::command]
pub async fn list_shares_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<crate::share::ShareList> {
    crate::share::list_shares(state.inner()).await.map_err(err)
}

#[tauri::command]
pub async fn create_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    node_ids: Vec<String>,
    quota_up_gb: u64,
    quota_down_gb: u64,
    rate_up_mbps: u32,
    rate_down_mbps: u32,
    expires_in_days: u32,
) -> R<crate::share::ShareInfo> {
    crate::share::create_share(
        state.inner(),
        crate::share::CreateShareInput {
            server_id,
            node_ids,
            options: crate::share::ShareOptions {
                quota_up_bytes: crate::share::quota_gb_to_bytes(quota_up_gb),
                quota_down_bytes: crate::share::quota_gb_to_bytes(quota_down_gb),
                rate_up_mbps,
                rate_down_mbps,
                expires_in_days,
            },
        },
    )
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn update_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
    quota_up_gb: u64,
    quota_down_gb: u64,
    rate_up_mbps: u32,
    rate_down_mbps: u32,
    expires_in_days: u32,
    node_ids: Option<Vec<String>>,
) -> R<crate::share::ShareInfo> {
    crate::share::update_share_with_nodes(
        state.inner(),
        server_id,
        &id,
        crate::share::ShareOptions {
            quota_up_bytes: crate::share::quota_gb_to_bytes(quota_up_gb),
            quota_down_bytes: crate::share::quota_gb_to_bytes(quota_down_gb),
            rate_up_mbps,
            rate_down_mbps,
            expires_in_days,
        },
        node_ids,
    )
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn reset_share_usage_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
) -> R<crate::share::ShareInfo> {
    crate::share::reset_usage(state.inner(), server_id, &id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn delete_share_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    id: String,
) -> R<()> {
    crate::share::delete_share(state.inner(), server_id, &id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn prepare_share_tls_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<crate::share::ShareTlsStatus> {
    crate::share::prepare_tls(state.inner(), server_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn share_tls_status_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<crate::share::ShareTlsStatus> {
    crate::share::tls_status(state.inner(), server_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn start_sub_server_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    port: u16,
    token: String,
) -> R<u16> {
    start_sub_server(state.inner().clone(), port, token).await
}

#[tauri::command]
pub async fn stop_sub_server_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<()> {
    stop_sub_server(state.inner().clone()).await
}

#[tauri::command]
pub async fn get_sub_server_status_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<bool> {
    get_sub_server_status(state.inner().clone()).await
}

#[tauri::command]
pub async fn get_settings_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    keys: Vec<String>,
) -> R<HashMap<String, String>> {
    get_settings(state.inner().clone(), keys)
}

#[tauri::command]
pub async fn set_setting_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    key: String,
    value: String,
) -> R<()> {
    set_setting(state.inner().clone(), key, value)
}

#[tauri::command]
pub async fn list_servers_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<Server>> {
    list_servers(state.inner().clone())
}
// 扁平参数形态（Tauri 按顶层键匹配参数名并做 camelCase→snake_case），
// 参数数量由线格式决定，此处仅做 State 提取与结构体组装
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_server_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
    host: String,
    host_v6: String,
    port: i64,
    user: String,
    auth_kind: String,
    kind: String,
    notes: String,
    credential: String,
    share_domain: String,
) -> R<Server> {
    create_server(
        state.inner().clone(),
        CreateServerInput {
            name,
            host,
            host_v6,
            port,
            user,
            auth_kind,
            kind,
            notes,
            credential,
            share_domain,
        },
    )
}
// 同 create_server_cmd：扁平参数形态，credential=None 表示保留原凭据
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn update_server_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    name: String,
    host: String,
    host_v6: String,
    port: i64,
    user: String,
    auth_kind: String,
    kind: String,
    notes: String,
    credential: Option<String>,
    egress_family: Option<String>,
    share_domain: Option<String>,
) -> R<UpdateServerResult> {
    update_server(
        state.inner().clone(),
        UpdateServerInput {
            id,
            name,
            host,
            host_v6,
            port,
            user,
            auth_kind,
            kind,
            notes,
            credential,
            egress_family,
            share_domain,
        },
    )
}
/// 服务器地址族切换：整台服务器（SSH/测速）与该机所有自建直连节点一起切。
#[tauri::command]
pub async fn set_server_addr_family_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    family: String,
) -> R<Server> {
    set_server_addr_family(state.inner().clone(), id, family).await
}
/// 从服务器自身探测全局 IPv6（SSH 走原始主机地址），返回顺序即推荐顺序。
#[tauri::command]
pub async fn detect_server_ipv6_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
) -> R<Vec<String>> {
    detect_server_ipv6(state.inner().clone(), id).await
}
#[tauri::command]
pub async fn delete_server_cmd(state: tauri::State<'_, Arc<AppState>>, id: i64) -> R<()> {
    delete_server(state.inner().clone(), id)
}
#[tauri::command]
pub async fn test_server_connection_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
) -> R<ConnectionTest> {
    test_server_connection(state.inner().clone(), id).await
}
#[tauri::command]
pub async fn setup_passwordless_cmd(state: tauri::State<'_, Arc<AppState>>, id: i64) -> R<()> {
    setup_passwordless(state.inner().clone(), id).await
}
#[tauri::command]
pub fn list_channels_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<Channel>> {
    list_channels(state.inner().clone(), server_id)
}
#[tauri::command]
pub fn add_channel_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    name: String,
    port: i64,
    protocol: String,
) -> R<Channel> {
    add_channel(
        state.inner().clone(),
        AddChannelInput {
            server_id,
            name,
            port,
            protocol,
        },
    )
}
#[tauri::command]
pub fn remove_channel_cmd(state: tauri::State<'_, Arc<AppState>>, id: i64) -> R<()> {
    remove_channel(state.inner().clone(), id)
}
#[tauri::command]
pub async fn channel_status_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
) -> R<ChannelStatus> {
    channel_status(state.inner().clone(), id).await
}
#[tauri::command]
pub async fn scan_server_ports_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<ServerPort>> {
    scan_server_ports(state.inner().clone(), server_id).await
}
#[tauri::command]
pub async fn channel_switch_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    enabled: bool,
) -> R<ChannelStatus> {
    set_channel_enabled(state.inner().clone(), id, enabled).await
}
#[tauri::command]
pub fn list_upstreams_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<Upstream>> {
    list_upstreams(state.inner().clone(), server_id)
}

/// realityDest 缺省时用与部署向导同一个默认伪装域名（两处不一致会让用户以为配置没生效）。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn add_upstream_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    name: String,
    proto: String,
    host: String,
    port: i64,
    username: String,
    password: String,
    listen_port: Option<i64>,
    reality_dest: Option<String>,
) -> R<Upstream> {
    add_upstream(
        state.inner().clone(),
        servers::upstream::UpstreamInput {
            server_id,
            name,
            proto,
            host,
            port,
            username,
            password,
            listen_port: listen_port.unwrap_or(servers::upstream::DEFAULT_LISTEN_PORT as i64),
            reality_dest: reality_dest
                .unwrap_or_else(|| servers::upstream::DEFAULT_REALITY_DEST.to_string()),
        },
    )
}

/// 编辑出口（含名称；改完要点「重新部署」才在远端生效）。
///
/// `password` 传 `null` 或空串 = **不改**（凭据不回显，表单拿不到原值）。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn update_upstream_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    id: i64,
    name: String,
    proto: String,
    host: String,
    port: i64,
    username: String,
    password: Option<String>,
    listen_port: i64,
    reality_dest: String,
) -> R<Upstream> {
    update_upstream(
        state.inner().clone(),
        id,
        servers::upstream::UpstreamEdit {
            name,
            proto,
            host,
            port,
            username,
            password,
            listen_port,
            reality_dest,
        },
    )
}

#[tauri::command]
pub fn remove_upstream_cmd(state: tauri::State<'_, Arc<AppState>>, id: i64) -> R<()> {
    remove_upstream(state.inner().clone(), id)
}

#[tauri::command]
pub fn start_upstream_job_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<i64> {
    start_upstream_job(state.inner().clone(), server_id)
}

#[tauri::command]
pub async fn probe_egress_ip_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    upstream_id: i64,
) -> R<String> {
    probe_egress_ip(state.inner().clone(), upstream_id).await
}

/// 打开某条线的隐私保护（Tor 隐藏服务入口）。返回部署 job 的 id。
#[tauri::command]
pub fn enable_privacy_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    node_id: String,
) -> R<i64> {
    enable_privacy(state.inner().clone(), server_id, node_id)
}

/// 关闭隐私保护。返回 `null` = 没提交作业（从未部署成功，记录已直接删掉）。
#[tauri::command]
pub fn disable_privacy_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    privacy_id: i64,
) -> R<Option<i64>> {
    disable_privacy(state.inner().clone(), privacy_id)
}

#[tauri::command]
pub fn list_privacies_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<Privacy>> {
    list_privacies(state.inner().clone())
}

#[tauri::command]
pub async fn start_deploy_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    protocols: Vec<String>,
    reality_dest: Option<String>,
    enable_bbr: Option<bool>,
) -> R<i64> {
    start_deploy(
        state.inner().clone(),
        DeployInput {
            server_id,
            protocols,
            reality_dest,
            enable_bbr,
        },
    )
}

/// 查看已部署协议（以本地自建节点为准）。
#[tauri::command]
pub async fn deployed_protocols_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<String>> {
    crate::servers::deploy::deployed_protocols(state.inner(), server_id).map_err(AppError::Message)
}

/// 查看已部署协议详情（协议变体 + 端口，以本地自建节点为准）。
#[tauri::command]
pub async fn deployed_protocol_details_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<Vec<crate::servers::deploy::DeployedProtocolDetail>> {
    crate::servers::deploy::deployed_protocol_details(state.inner(), server_id)
        .map_err(AppError::Message)
}

/// 补装协议（现有 ∪ 勾选 提交 deploy job）。
#[tauri::command]
pub async fn install_supplement_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    protocols: Vec<String>,
) -> R<i64> {
    install_supplement(state.inner().clone(), server_id, protocols)
}

/// 卸载协议（骨架：P2 里程碑实现停止 unit + 摘除入站 + 重写摘要）。
#[tauri::command]
pub async fn remove_protocol_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    protocol: String,
) -> R<()> {
    remove_protocol(state.inner().clone(), server_id, protocol)
}
#[tauri::command]
pub async fn list_jobs_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<Job>> {
    list_jobs(state.inner().clone())
}

// ---------- 本机上网 ----------

pub async fn net_status(state: Arc<AppState>) -> R<crate::net::NetStatus> {
    // 服务不可达时 status() 返回 Err（未知 ≠ 没在跑）：如实抛给界面，
    // 前端据此走「状态未知」分支并禁用开关，而不是显示一个假的「未开启」。
    crate::net::runtime::status(&state)
        .await
        .map_err(AppError::Message)
}

pub async fn net_start(
    state: Arc<AppState>,
    input: crate::domain::NetStartInput,
) -> R<crate::net::NetStatus> {
    crate::net::runtime::start(
        &state,
        crate::net::StartOptions {
            ignore_conflicts: input.ignore_conflicts,
            conflict_override: input.conflict_override,
        },
    )
    .await
    .map_err(AppError::Message)
}

pub async fn net_stop(state: Arc<AppState>) -> R<crate::net::NetStatus> {
    crate::net::runtime::stop(&state)
        .await
        .map_err(AppError::Message)
}

/// 上网状态（仪表盘网络卡挂载时拉取）。
#[tauri::command]
pub async fn net_status_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<crate::net::NetStatus> {
    net_status(state.inner().clone()).await
}

#[tauri::command]
pub async fn net_repair_proxy_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<()> {
    crate::net::runtime::repair_system_proxy(state.inner())
        .await
        .map_err(AppError::Message)
}

/// 一键开启：冲突即拒（提醒不抢占），由调用方传 ignore_conflicts/conflict_override。
#[tauri::command]
pub async fn net_start_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    ignore_conflicts: bool,
    conflict_override: Option<Vec<String>>,
) -> R<crate::net::NetStatus> {
    net_start(
        state.inner().clone(),
        crate::domain::NetStartInput {
            ignore_conflicts,
            conflict_override,
        },
    )
    .await
}

/// 一键关闭：杀 mihomo + 还原系统代理。
#[tauri::command]
pub async fn net_stop_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<crate::net::NetStatus> {
    net_stop(state.inner().clone()).await
}

/// 提权服务的安装/运行状态（设置页挂载时拉取）。
#[tauri::command]
pub async fn net_service_state_cmd() -> R<crate::net::ServiceState> {
    Ok(crate::net::ServiceState::query())
}

/// 以管理员身份安装网络服务（**全程唯一一次 UAC**）。
///
/// 只负责把提权助手拉起来，不等它结果——UAC 窗口是模态的，等下去会把主线程卡住。
/// 界面靠轮询 `net_service_state_cmd` 看结果，而不是假定成功。
#[tauri::command]
pub async fn net_service_install_cmd() -> R<()> {
    #[cfg(windows)]
    {
        crate::service::install::run_elevated("--install-service").map_err(AppError::Message)
    }
    #[cfg(not(windows))]
    {
        Err(AppError::Message("网络服务仅 Windows 可用".into()))
    }
}

/// 以管理员身份卸载网络服务（同样过一次 UAC）。
#[tauri::command]
pub async fn net_service_uninstall_cmd() -> R<()> {
    #[cfg(windows)]
    {
        crate::service::install::run_elevated("--uninstall-service").map_err(AppError::Message)
    }
    #[cfg(not(windows))]
    {
        Err(AppError::Message("网络服务仅 Windows 可用".into()))
    }
}

/// 开关 TUN 全局模式。**没有降级路径**：建网卡要管理员令牌，服务不可用就报错。
#[tauri::command]
pub async fn net_set_tun_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    enable: bool,
) -> R<crate::net::NetStatus> {
    crate::net::runtime::set_tun(state.inner(), enable)
        .await
        .map_err(AppError::Message)?;
    // 回读状态而不是就地构造：TUN 是否真的开上了由服务说了算，
    // 让界面拿到的是事实而非我们的假设
    net_status(state.inner().clone()).await
}

/// 手动切换出口节点（把「手动选择」组改为指定节点名；「自动选择」= 交回测速最快）。
///
/// 顺带记下这次的模式：用户动作是「自动还是手动」唯一可信的来源——自动挑选走的也是同
/// 一条写组路径，光看组里现在是谁分不出两者（见 `runtime::EXIT_MODE_KEY`）。
#[tauri::command]
pub async fn net_select_cmd(state: tauri::State<'_, Arc<AppState>>, node_name: String) -> R<()> {
    let st = state.inner();
    crate::net::runtime::select(st, &node_name)
        .await
        .map_err(AppError::Message)?;
    // 只有控制器真的切换成功才记住用户模式；隐私核验拒绝时旧出口仍在。
    crate::net::runtime::remember_exit_mode(st, &node_name);
    // 选回「自动选择」时立刻按实测结果落地，别先退回按延迟排的 url-test 等下次测速——
    // 那正是这个选项要避免的事。挑不出来（没读数/全过期）就停在 url-test，由测速纠正。
    if node_name == crate::config::AUTO_GROUP {
        let _ = crate::net::runtime::apply_fastest_measured(st).await;
    }
    Ok(())
}

/// 运行时连接日志 + 泄漏检测快照（第 7 项）。
///
/// 返回的是采样器**当前窗口**，不触发新一轮采样：面板打开时先拿它铺满列表，
/// 之后靠 `net://conn_log` 事件增量刷新。`only_leaks` 只看有证据的泄漏，
/// Suspect 单列——把「拿不准」混进「泄漏」里，用户很快就会学会无视整个面板。
#[tauri::command]
pub async fn net_conn_log_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    limit: Option<usize>,
    only_leaks: bool,
) -> R<crate::net::connlog::ConnLogSnapshot> {
    Ok(state.conn_sampler.snapshot(limit, only_leaks))
}

/// 清除残留的会话阻断规则（设置页逃生门）。
///
/// 正常路径上不该有残留：关闭网络会无条件撤、开启网络会自愈上一次强杀留下的。
/// 这个按钮服务的是「上次被强杀、现在还没开网络、但想立刻把残留清掉」——
/// 残留的 BLOCK 规则是**真会挡住用户网络**的，所以必须给一条不依赖任何自动
/// 路径的手工出口。网络运行中会拒绝（那时规则正在生效，清掉等于界面撒谎）。
#[tauri::command]
pub async fn net_firewall_clear_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<crate::net::FirewallClearResult> {
    crate::net::runtime::clear_firewall(state.inner())
        .await
        .map_err(AppError::Message)
}

/// 当前手动选择的分组出口名（仪表盘下拉回显）。
#[tauri::command]
pub async fn net_current_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<String> {
    crate::net::runtime::current(state.inner())
        .await
        .map_err(AppError::Message)
}
#[tauri::command]
pub async fn cancel_job_cmd(state: tauri::State<'_, Arc<AppState>>, job_id: i64) -> R<()> {
    cancel_job(state.inner().clone(), job_id)
}

// ---------- 中转链路壳 ----------

#[tauri::command]
pub async fn list_relay_rules_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<RelayRule>> {
    list_relay_rules(state.inner().clone())
}

#[tauri::command]
pub async fn probe_link_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    relay_server_id: i64,
    landing_server_id: i64,
) -> R<ProbeResult> {
    probe_link(state.inner().clone(), relay_server_id, landing_server_id).await
}

// 同 create_server_cmd：扁平参数形态，9 个业务参数由线格式决定，此处仅做 State 提取与结构体组装
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn build_link_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
    relay_server_id: i64,
    landing_server_id: i64,
    client_mode: String,
    wg_port: u16,
    forward_port: Option<u16>,
    landing_port: u16,
    converged: bool,
    action: String,
    forward_mode: Option<String>,
) -> R<i64> {
    build_link(
        state.inner().clone(),
        BuildLinkInput {
            name,
            relay_server_id,
            landing_server_id,
            client_mode,
            wg_port,
            forward_port,
            landing_port,
            converged,
            action,
            // 旧前端不带这个键 → None = 未指定：沿用已有规则的粒度、新建才回落 single
            forward_mode,
        },
    )
}

// 同 create_server_cmd：扁平参数形态，11 个业务参数由线格式决定，此处仅做 State 提取与结构体组装
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn adopt_link_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
    relay_server_id: i64,
    landing_server_id: i64,
    client_mode: String,
    wg_port: u16,
    forward_port: Option<u16>,
    relay_pubkey: Option<String>,
    landing_pubkey: Option<String>,
    landing_port: u16,
    subnet: String,
    converged: bool,
) -> R<RelayRule> {
    adopt_link(
        state.inner().clone(),
        AdoptLinkInput {
            name,
            relay_server_id,
            landing_server_id,
            client_mode,
            wg_port,
            forward_port,
            relay_pubkey,
            landing_pubkey,
            landing_port,
            subnet,
            converged,
        },
    )
    .await
}

#[tauri::command]
pub async fn teardown_link_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    rule_id: i64,
    action: String,
) -> R<i64> {
    teardown_link(state.inner().clone(), rule_id, action)
}

// 同 start_deploy_cmd：扁平参数形态（nodeIds/concurrency），壳函数只做 State 提取与委托
#[tauri::command]
pub async fn speed_test_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    node_ids: Vec<String>,
    concurrency: u8,
) -> R<i64> {
    speed_test(state.inner().clone(), node_ids, concurrency)
}

// 服务器测速：扁平参数形态（serverIds/concurrency），壳函数只做 State 提取与委托
#[tauri::command]
pub async fn server_speed_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_ids: Vec<i64>,
    concurrency: u8,
) -> R<i64> {
    server_speed(state.inner().clone(), server_ids, concurrency)
}

#[tauri::command]
pub async fn server_throughput_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    from_server_id: i64,
    to_server_id: i64,
    seconds: u8,
) -> R<i64> {
    server_throughput(state.inner().clone(), from_server_id, to_server_id, seconds)
}

#[tauri::command]
pub async fn install_iperf3_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
) -> R<String> {
    install_iperf3(state.inner().clone(), server_id).await
}

#[tauri::command]
pub async fn list_throughput_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<Vec<ThroughputTest>> {
    list_throughput(state.inner().clone())
}

// BBR 检测/开启：serverId 线格式，壳函数只做 State 提取与委托
#[tauri::command]
pub async fn check_bbr_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<String> {
    check_bbr(state.inner().clone(), server_id).await
}

#[tauri::command]
pub async fn enable_bbr_cmd(state: tauri::State<'_, Arc<AppState>>, server_id: i64) -> R<String> {
    enable_bbr(state.inner().clone(), server_id).await
}

/// 重新接管：failed 半成品行一键恢复（回读公钥 + 置 imported + 节点改指）。
#[tauri::command]
pub async fn readopt_link_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    rule_id: i64,
) -> R<RelayRule> {
    relay::readopt_link(state.inner(), rule_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn delete_link_rule_cmd(state: tauri::State<'_, Arc<AppState>>, rule_id: i64) -> R<()> {
    delete_link_rule(state.inner().clone(), rule_id)
}

#[tauri::command]
pub async fn refresh_link_status_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    rule_id: i64,
) -> R<LinkStatus> {
    refresh_link_status(state.inner().clone(), rule_id).await
}

#[tauri::command]
pub async fn list_device_peers_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    rule_id: i64,
) -> R<Vec<DevicePeer>> {
    list_device_peers(state.inner().clone(), rule_id)
}

#[tauri::command]
pub async fn add_device_peer_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    rule_id: i64,
    name: String,
) -> R<i64> {
    add_device_peer(state.inner().clone(), rule_id, name)
}

#[tauri::command]
pub async fn remove_device_peer_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: i64,
) -> R<i64> {
    remove_device_peer(state.inner().clone(), peer_id)
}

#[tauri::command]
pub async fn get_peer_config_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    peer_id: i64,
) -> R<String> {
    get_peer_config(state.inner().clone(), peer_id).await
}

#[tauri::command]
pub async fn server_monitor_summary_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<MonitorSummary> {
    monitor_summary(state.inner().clone())
}

#[tauri::command]
pub async fn server_monitor_history_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    server_id: i64,
    metric: String,
    range_secs: i64,
) -> R<Vec<MonitorPoint>> {
    monitor_history(state.inner().clone(), server_id, metric, range_secs)
}

#[tauri::command]
pub async fn server_monitor_alerts_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    include_resolved: bool,
    limit: u32,
) -> R<Vec<MonitorAlert>> {
    monitor_alerts(state.inner().clone(), include_resolved, limit)
}

#[tauri::command]
pub async fn server_monitor_collect_cmd(
    state: tauri::State<'_, Arc<AppState>>,
) -> R<MonitorSummary> {
    monitor_collect(state.inner().clone()).await
}

#[tauri::command]
pub async fn server_monitor_config_cmd(state: tauri::State<'_, Arc<AppState>>) -> R<MonitorConfig> {
    monitor_config(state.inner().clone())
}

#[tauri::command]
pub async fn server_monitor_set_config_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    config: MonitorConfig,
) -> R<MonitorConfig> {
    monitor_set_config(state.inner().clone(), config)
}

#[tauri::command]
pub async fn server_monitor_ack_cmd(
    state: tauri::State<'_, Arc<AppState>>,
    alert_id: i64,
) -> R<()> {
    monitor_ack(state.inner().clone(), alert_id)
}
