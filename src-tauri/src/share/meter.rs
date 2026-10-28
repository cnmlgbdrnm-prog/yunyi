//! Per-share cgroup IP accounting. Missing support must fail, never estimate.
use super::{model::ShareManifest, scripts};
use crate::servers::deploy::{run_cmd_ok, write_remote_file, CONFIG_TIMEOUT};
use crate::ssh::SshSession;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub fn unit(id: &str) -> String {
    format!("[Unit]\nDescription=vpn-ctl share {id} public IP meter\nAfter=network.target\n\n[Service]\nType=notify\nNotifyAccess=main\nWatchdogSec=10\nExecStart=/usr/bin/python3 /srv/vpnshare/share_meter.py serve {id}\nRestart=on-failure\nRestartSec=3\nTimeoutStartSec=30\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n")
}

pub fn slice(id: &str) -> String {
    format!("[Unit]\nDescription=vpn-ctl share {id} accounting group\nRequires=vpnshare-{id}-meter.service\nAfter=vpnshare-{id}-meter.service\n\n[Slice]\nIPIngressFilterPath=/sys/fs/bpf/vpnshare/{id}/programs/share_ingress\nIPEgressFilterPath=/sys/fs/bpf/vpnshare/{id}/programs/share_egress\n")
}

pub fn binding(id: &str) -> String {
    format!("[Unit]\nBindsTo=vpnshare-{id}-meter.service\nAfter=vpnshare-{id}-meter.service\n\n[Service]\nSlice=vpnshare-{id}.slice\n")
}

pub async fn install(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    manifest: &ShareManifest,
) -> Result<(), String> {
    if manifest.quota_total_bytes.is_none() {
        return Ok(());
    }
    let id = &manifest.id;
    run_cmd_ok(sess,cancel,"公网计量依赖",
        "test $(stat -fc %T /sys/fs/cgroup) = cgroup2fs && { command -v clang >/dev/null && bpftool version >/dev/null 2>&1 || { command -v apt-get >/dev/null && export DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=l && apt-get update -qq && apt-get install -y --no-install-recommends clang linux-libc-dev && { bpftool version >/dev/null 2>&1 || apt-get install -y --no-install-recommends bpftool || apt-get install -y --no-install-recommends linux-tools-$(uname -r); }; }; } && bpftool version >/dev/null",
        Duration::from_secs(600)).await?;
    for (path, content) in [
        ("/srv/vpnshare/share_meter.py", scripts::SHARE_METER_PY),
        ("/srv/vpnshare/share_meter.bpf.c", scripts::SHARE_METER_BPF),
    ] {
        write_remote_file(sess, cancel, "安装公网计量", path, content, CONFIG_TIMEOUT).await?;
    }
    run_cmd_ok(
        sess,
        cancel,
        "编译公网计量",
        "python3 /srv/vpnshare/share_meter.py prepare",
        Duration::from_secs(60),
    )
    .await?;
    write_remote_file(
        sess,
        cancel,
        "安装公网计量",
        &format!("/etc/systemd/system/vpnshare-{id}-meter.service"),
        &unit(id),
        CONFIG_TIMEOUT,
    )
    .await?;
    write_remote_file(
        sess,
        cancel,
        "安装公网计量",
        &format!("/etc/systemd/system/vpnshare-{id}.slice"),
        &slice(id),
        CONFIG_TIMEOUT,
    )
    .await?;
    for suffix in ["", "-gateway", "-xray", "-singbox", "-ss"] {
        write_remote_file(
            sess,
            cancel,
            "绑定公网计量",
            &format!("/etc/systemd/system/vpnshare-{id}{suffix}.service.d/90-share-meter.conf"),
            &binding(id),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    run_cmd_ok(
        sess,
        cancel,
        "启动公网计量",
        &format!("systemctl daemon-reload && systemctl enable --now vpnshare-{id}-meter.service"),
        Duration::from_secs(45),
    )
    .await?;
    Ok(())
}

pub async fn sync(
    sess: &mut SshSession,
    cancel: &AtomicBool,
    manifest: &ShareManifest,
) -> Result<(), String> {
    if manifest.quota_total_bytes.is_some() {
        run_cmd_ok(
            sess,
            cancel,
            "同步总额度",
            &format!("python3 /srv/vpnshare/share_meter.py sync {}", manifest.id),
            CONFIG_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

pub async fn remove(sess: &mut SshSession, cancel: &AtomicBool, id: &str) -> Result<(), String> {
    run_cmd_ok(
        sess,
        cancel,
        "删除公网计量",
        &format!("if test -f /etc/systemd/system/vpnshare-{id}-meter.service || test -f /var/lib/vpnshare-meters/{id}.json; then python3 /srv/vpnshare/share_meter.py remove {id}; fi"),
        Duration::from_secs(60),
    )
    .await?;
    Ok(())
}
