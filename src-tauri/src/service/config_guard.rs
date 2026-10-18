//! 落盘前的配置清洗。
//!
//! 威胁模型：配置内容由**非提权**的主程序提供，而 mihomo 以 SYSTEM 运行。
//! mihomo 配置里能写文件、能挂外部资源的字段就是提权路径。本模块只做减法，
//! 另有一处**覆写**（geodata-dir），其余语义一概不改。
//!
//! `geodata-dir` 为什么是覆写而非报错：geo 数据集是**服务自有的只读资产**
//! （随安装包分发，由 mihomo.rs 的 provision 铺到位），不是用户数据。它必须落在
//! 管理员可写的位置——否则非提权进程能换掉 SYSTEM mihomo 要解析的数据文件。
//! 因此调用方传什么都不作数，一律按服务自己的固定目录钉死（spec §4.2 第 3 条）。
//! 这与 `log-file` 同理：该字段本就不由调用方掌控。
//!
//! 曾经的写法是「越界报错」，实测**必然误伤**：主程序生成的配置里 geodata-dir
//! 恒指向它自己的数据目录（`config/mod.rs` 无条件插入），与服务的固定目录永不
//! 相等，于是服务每次都返回 Failed。而主程序对「服务活着却失败」的契约是不降级
//! （见 net/runtime.rs），结果就是装上服务后「开启网络」100% 失败。
//!
//! 已知局限（照实写在这，别当成已解决）：这是**黑名单**，对 mihomo 未来新增的
//! 「能写文件」字段天然失效。要根治得让服务自己生成配置，而节点数据来自订阅
//! （且手工节点可带 clash_raw 片段直插配置，见 config/mod.rs 的 node_mapping），
//! 源头同样不可信，故黑名单是当前取舍下的最优解。
//!
//! 输入输出：YAML 文本 + 钉死的 geo 目录 → 清洗后的 YAML 文本；纯函数，可跨平台测试。
//! 依赖：serde_yaml。

use std::path::Path;

/// 必须删除的顶层字段：它们能让 SYSTEM 身份的 mihomo 写任意路径。
const STRIPPED_KEYS: [&str; 2] = ["log-file", "external-ui"];

/// 清洗配置：删危险字段 + 把 geodata-dir 覆写为 `pinned_geo_dir`。
pub fn sanitize(config_yaml: &str, pinned_geo_dir: &Path) -> Result<String, String> {
    let mut root: serde_yaml::Value =
        serde_yaml::from_str(config_yaml).map_err(|e| format!("配置不是合法 YAML: {e}"))?;
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| "配置顶层必须是映射（key: value），实际不是".to_string())?;

    for key in STRIPPED_KEYS {
        map.remove(serde_yaml::Value::from(key));
    }

    // 无条件覆写（含调用方压根没给这个字段的情况）：服务自有的数据集归服务管
    map.insert(
        serde_yaml::Value::from("geodata-dir"),
        serde_yaml::Value::from(pinned_geo_dir.to_string_lossy().to_string()),
    );

    serde_yaml::to_string(&root).map_err(|e| format!("清洗后序列化失败: {e}"))
}
