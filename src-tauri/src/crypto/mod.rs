//! 凭据加密模块：随机 32 字节数据密钥由 KeyStore 持久化（生产用 Windows 凭据管理器，
//! 其底层 DPAPI 按 Windows 账户隔离），凭据以 AES-256-GCM 加密（nonce‖密文 格式）。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use std::sync::Arc;
use thiserror::Error;

/// 加密模块错误：密钥库失败 / 加密失败 / 解密失败（含篡改与错误密钥）。
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("密钥库不可用: {0}")]
    KeyStore(String),
    #[error("加密失败: {0}")]
    Encrypt(String),
    #[error("解密失败：数据已损坏或密钥不匹配")]
    Decrypt,
}

/// 数据密钥存储抽象：生产为 Windows 凭据管理器，测试为内存实现。
pub trait KeyStore: Send + Sync {
    fn get(&self) -> Result<[u8; 32], CryptoError>;
    fn set(&self, key: &[u8; 32]) -> Result<(), CryptoError>;
    /// 条目是否存在：区分"无密钥"与"密钥存在但不可读"，
    /// 后者必须报错而不是重新生成密钥覆盖（否则旧凭据全部无法解密）。
    fn exists(&self) -> Result<bool, CryptoError>;
}

/// 生产实现：keyring → Windows 凭据管理器（service 名固定 vpn-ctl）。
pub struct KeyringStore;

impl KeyStore for KeyringStore {
    fn get(&self) -> Result<[u8; 32], CryptoError> {
        let entry = keyring::Entry::new("vpn-ctl", "data-key")
            .map_err(|e| CryptoError::KeyStore(e.to_string()))?;
        let b64 = entry
            .get_password()
            .map_err(|e| CryptoError::KeyStore(e.to_string()))?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .map_err(|_| CryptoError::KeyStore("密钥库数据不是合法 base64".into()))?;
        bytes
            .try_into()
            .map_err(|_| CryptoError::KeyStore("密钥库数据长度错误".into()))
    }

    fn set(&self, key: &[u8; 32]) -> Result<(), CryptoError> {
        let entry = keyring::Entry::new("vpn-ctl", "data-key")
            .map_err(|e| CryptoError::KeyStore(e.to_string()))?;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key);
        entry
            .set_password(&b64)
            .map_err(|e| CryptoError::KeyStore(e.to_string()))
    }

    fn exists(&self) -> Result<bool, CryptoError> {
        let entry = keyring::Entry::new("vpn-ctl", "data-key")
            .map_err(|e| CryptoError::KeyStore(e.to_string()))?;
        match entry.get_password() {
            Ok(_) => Ok(true),
            // keyring 3 的 NoEntry 表示凭据条目不存在（Windows 凭据管理器同样走此变体），
            // 其余错误（权限、平台失败等）视为不可读而非不存在
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(CryptoError::KeyStore(e.to_string())),
        }
    }
}

/// 测试实现：进程内存储。
// 仅在测试目标构造（commands_tests / crypto::tests），bin 目标无构造点，予以豁免。
// 测试源码已移出仓库（tests/rust，gitignore），只有 local-tests 特性下才会被
// include! 进来；因此豁免条件必须带上该特性，否则未启用特性时 clippy
// --all-targets 会因 cfg(test) 成立却无测试模块而报 dead_code。
#[cfg_attr(not(all(test, feature = "local-tests")), allow(dead_code))]
#[derive(Default)]
pub struct MemoryStore {
    key: std::sync::Mutex<Option<[u8; 32]>>,
}

impl KeyStore for MemoryStore {
    fn get(&self) -> Result<[u8; 32], CryptoError> {
        self.key
            .lock()
            .map_err(|_| CryptoError::KeyStore("内存锁中毒".into()))?
            .ok_or_else(|| CryptoError::KeyStore("密钥不存在".into()))
    }

    fn set(&self, key: &[u8; 32]) -> Result<(), CryptoError> {
        *self
            .key
            .lock()
            .map_err(|_| CryptoError::KeyStore("内存锁中毒".into()))? = Some(*key);
        Ok(())
    }

    fn exists(&self) -> Result<bool, CryptoError> {
        Ok(self
            .key
            .lock()
            .map_err(|_| CryptoError::KeyStore("内存锁中毒".into()))?
            .is_some())
    }
}

/// 降级痕迹上限：只留最近若干条**不同原因**，防止一条坏数据把内存与日志刷爆。
const MAX_DEGRADATION_TRACES: usize = 32;

/// 降级痕迹落盘位置（进程级，main 装配时设置一次；与 [`crate::ssh::set_known_hosts_path`] 同形）。
///
/// 为什么必须落盘：打包后没有控制台，只 `eprintln` 等于没人看得见——main.rs 已经为
/// `startup_config_error` 记过同一课（「失败只进 stderr 在打包后不可见」）。
static DOWNGRADE_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 降级痕迹文件名（数据目录下）。启动告警要按同一个名字去找它，所以是常量。
pub const DEGRADATION_LOG_NAME: &str = "crypto_degradations.log";

/// 设置降级痕迹文件路径（main 启动时调用；重复调用忽略）。
pub fn set_downgrade_log_path(path: std::path::PathBuf) {
    let _ = DOWNGRADE_LOG.set(path);
}

/// 追加一行降级痕迹（RFC3339 时间 + 原因）。写不进去也不影响主流程——痕迹是诊断，不是业务。
/// `store` 显式传入以便单测：进程级全局在并行测试里无法安全复位。
pub(crate) fn append_degradation_log(store: Option<&std::path::Path>, reason: &str) {
    use std::io::Write;
    let Some(path) = store else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{}\t{reason}\n", chrono::Utc::now().to_rfc3339());
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// 凭据加密服务：持有数据密钥，提供 seal/open。
pub struct CredentialService {
    cipher: Aes256Gcm,
    /// 降级痕迹（见 [`open_param`]）：解不开的密文被原样返回时，这里是唯一能指出原因的地方。
    /// 每个原因只记一次——节点列表每次轮询都会重新解密全部节点。
    degradations: std::sync::Mutex<Vec<String>>,
}

impl CredentialService {
    /// 创建服务；密钥库无密钥时生成随机 32 字节数据密钥并持久化。
    /// 区分"无密钥"与"密钥存在但不可读"：条目存在时绝不生成新密钥覆盖，
    /// 否则数据密钥轮换会让既有凭据全部无法解密。
    pub fn new(store: Arc<dyn KeyStore>) -> Result<Self, CryptoError> {
        let key = if store.exists()? {
            store.get()?
        } else {
            let fresh: [u8; 32] = rand::random();
            store.set(&fresh)?;
            fresh
        };
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        Ok(Self {
            cipher,
            degradations: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// 记一条降级痕迹：内存一条 + stderr + 落盘，**同原因只记一次**。
    ///
    /// 去重不是省事：节点列表每轮询一次就会把所有节点重新解密一遍，逐条追加会让痕迹和
    /// 日志文件无限增长，几次轮询之后没人能读它——等于又变回没有痕迹。
    pub fn note_degradation(&self, reason: &str) {
        {
            let mut traces = crate::util::lock_ignore_poison(&self.degradations);
            if traces.iter().any(|t| t == reason) || traces.len() >= MAX_DEGRADATION_TRACES {
                return;
            }
            traces.push(reason.to_string());
        }
        eprintln!("[vpn-ctl] 凭据加密降级: {reason}");
        append_degradation_log(DOWNGRADE_LOG.get().map(|p| p.as_path()), reason);
    }

    /// 已记录的降级痕迹（空＝未发生过降级）。
    ///
    /// 痕迹的用户可见部分是落盘文件与 stderr；这个读口给测试与将来的界面用。
    /// 之所以不把痕迹做成进程级全局（那样非测试也能读）：并行测试会互相污染，
    /// 而「谁降级了」本就该按服务实例算。
    // 当前 bin 目标无读取点（与 MemoryStore 同形），予以豁免
    #[cfg_attr(not(all(test, feature = "local-tests")), allow(dead_code))]
    pub fn degradations(&self) -> Vec<String> {
        crate::util::lock_ignore_poison(&self.degradations).clone()
    }

    /// 加密：输出 = 12 字节随机 nonce ‖ 密文。
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| CryptoError::Encrypt(e.to_string()))?;
        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 解密：解析 nonce‖密文；认证失败统一报 Decrypt（含篡改与错误密钥）。
    pub fn open(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if blob.len() < 12 {
            return Err(CryptoError::Decrypt);
        }
        let (nonce_bytes, ct) = blob.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, ct)
            .map_err(|_| CryptoError::Decrypt)
    }
}

/// 节点参数存储格式前缀：`enc:v1:` + base64(nonce‖密文)；无前缀视为历史明文
/// （升级前数据兼容读，下次写入自动加密）。
pub const PARAM_PREFIX: &str = "enc:v1:";

/// 节点参数加密：seal → base64 → `enc:v1:` 前缀。空串原样（无可保内容）。
/// 加密失败时降级明文（宁可写不加密也不能让订阅同步/自建回写整链回滚；读侧无前缀
/// 即按明文处理，语义自洽），但**必须留痕**——见 [`CredentialService::note_degradation`]。
///
/// 这条 Err 分支今天实际到不了：AES-GCM 只在明文超过 2^36 字节时返回错误
/// （aes-gcm-0.10.3/src/lib.rs:127 `P_MAX = 1 << 36`），而**密钥库不可用根本走不到这里**
/// ——`CredentialService::new` 就已经返回 Err（state.rs:79 → main.rs 装配 expect：启动即失败，
/// 是响的不是静的）。留着它是因为这条路的后果是「凭据落库变明文」，将来换密码学实现
/// 或放宽输入上限时它会变成活路，那时痕迹必须已经在。
pub fn seal_param(creds: &CredentialService, plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    match creds.seal(plaintext.as_bytes()) {
        Ok(blob) => format!(
            "{PARAM_PREFIX}{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, blob)
        ),
        Err(e) => {
            creds.note_degradation(&format!("参数加密失败，已按明文写库：{e}"));
            plaintext.to_string()
        }
    }
}

/// 节点参数解密：`enc:v1:` 前缀 → base64 解码 + open；失败/无前缀 → 原样返回
/// （legacy 明文兼容）。
///
/// **解不开时必须留痕**：返回值是原样的密文串，而它与 legacy 明文从调用方看毫无区别，
/// 上游唯一能看到的症状是 JSON 解析失败（`expected value at line 1 column 1`）——追查方向
/// 全指向 JSON，没人会想到是密钥不匹配。本仓真出现过这个症状（旧二进制跑新版迁移过的库），
/// 那一次的排查成本就是因为这里没有任何痕迹。两种失败分开记：编码坏了与密钥不匹配
/// 是两条完全不同的排查路线。
pub fn open_param(creds: &CredentialService, stored: &str) -> String {
    let Some(b64) = stored.strip_prefix(PARAM_PREFIX) else {
        return stored.to_string();
    };
    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(b) => b,
        Err(e) => {
            creds.note_degradation(&format!("凭据密文本体不是合法 base64（{e}），已按原串返回"));
            return stored.to_string();
        }
    };
    match creds.open(&bytes) {
        Ok(plain) => String::from_utf8_lossy(&plain).into_owned(),
        Err(_) => {
            // CryptoError::Decrypt 的文案本身就是「解密失败：…」，这里只补它没有的信息
            creds.note_degradation(
                "凭据解密失败，已按原串返回：数据密钥与写入时不一致，或密文被损坏/篡改",
            );
            stored.to_string()
        }
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/crypto/mods.rs"
));
