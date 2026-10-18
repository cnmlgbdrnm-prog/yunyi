//! 数据库模块：连接管理、迁移入口、表仓库。单进程桌面应用用 Mutex<Connection> 即可。

pub mod migrations;
pub mod repos;

use rusqlite::Connection;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// 数据库句柄：WAL + 外键约束，连接由互斥锁保护。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// 打开（必要时创建）数据库文件并执行迁移。
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 获取连接守卫；调用方按最小作用域使用。
    ///
    /// 忽略锁毒化（见 [`crate::util::lock_ignore_poison`]）：原先这里是
    /// `.expect("db mutex poisoned")`，一次持锁 panic 之后**每个**碰数据库的命令都会
    /// 跟着 panic——应用等于慢慢死掉，而 SQLite 连接本身没有「半更新」的毒化语义。
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        crate::util::lock_ignore_poison(&self.conn)
    }
}

// 测试模块源码位于仓库根 tests/rust（不随 git 上传）；本地运行需启用 local-tests 特性。
#[cfg(all(test, feature = "local-tests"))]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/rust/db/mods.rs"
));
