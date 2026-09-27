//! Profile ごとのローカル状態（SQLite）。
//!
//! 転送の再開に必要な情報（送信元ファイルのパス、受信済みチャンク）と UI 用の履歴を持つ。
//! どのチャンクがアップロード済みかの正はサーバー側にあり、ここは「ローカルでしか分からないこと」だけを保存する。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tsute_proto::Transfer;

use crate::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalStatus {
    /// 送信: アップロード中 / 受信: ダウンロード中
    Active,
    /// 送信: 全アップロード完了・受信待ち
    Uploaded,
    /// 送受信とも完了
    Done,
    Cancelled,
    Failed,
}

impl LocalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Uploaded => "uploaded",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "uploaded" => Self::Uploaded,
            "done" => Self::Done,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub transfer: Transfer,
    pub direction: Direction,
    pub status: LocalStatus,
    pub error: Option<String>,
    /// 受信: 保存先パス（ファイル順）、送信: 送信元パス
    pub paths: Vec<PathBuf>,
    pub updated_at: i64,
}

/// 転送に含まれる 1 ファイルのローカル情報
#[derive(Debug, Clone)]
pub struct FileRow {
    /// 送信: 送信元パス / 受信: 最終的な保存先パス
    pub path: PathBuf,
    /// 送信: 開始時の mtime(ns)
    pub mtime_ns: Option<i64>,
    /// 受信: 書き込み中の part ファイル
    pub part: Option<PathBuf>,
    /// 受信: 検証・確定済み
    pub done: bool,
}

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS transfers (
    transfer_id TEXT PRIMARY KEY,
    direction   TEXT NOT NULL,
    status      TEXT NOT NULL,
    error       TEXT,
    json        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS transfer_files (
    transfer_id TEXT NOT NULL,
    file_idx    INTEGER NOT NULL,
    path        TEXT NOT NULL,
    -- 送信: 開始時の mtime(ns)。再開時に元ファイルが変更されていないかの確認に使う
    mtime_ns    INTEGER,
    part_path   TEXT,
    done        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (transfer_id, file_idx)
);
CREATE TABLE IF NOT EXISTS incoming_chunks (
    transfer_id TEXT NOT NULL,
    file_idx    INTEGER NOT NULL,
    chunk_idx   INTEGER NOT NULL,
    PRIMARY KEY (transfer_id, file_idx, chunk_idx)
);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db lock")
    }

    pub fn upsert_transfer(&self, t: &Transfer, dir: Direction, status: LocalStatus) -> Result<(), Error> {
        let now = crate::api::now();
        self.c().execute(
            "INSERT INTO transfers(transfer_id, direction, status, json, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(transfer_id) DO UPDATE SET json=excluded.json, updated_at=excluded.updated_at",
            params![
                t.transfer_id,
                if dir == Direction::Outgoing { "out" } else { "in" },
                status.as_str(),
                serde_json::to_string(t).expect("json"),
                t.created_at,
                now
            ],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: LocalStatus, error: Option<&str>) -> Result<(), Error> {
        self.c().execute(
            "UPDATE transfers SET status=?2, error=?3, updated_at=?4 WHERE transfer_id=?1",
            params![id, status.as_str(), error, crate::api::now()],
        )?;
        Ok(())
    }

    pub fn status(&self, id: &str) -> Result<Option<LocalStatus>, Error> {
        Ok(self
            .c()
            .query_row("SELECT status FROM transfers WHERE transfer_id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|s| LocalStatus::parse(&s)))
    }

    pub fn set_file(
        &self,
        id: &str,
        file: u32,
        path: &Path,
        mtime_ns: Option<i64>,
        part: Option<&Path>,
    ) -> Result<(), Error> {
        self.c().execute(
            "INSERT OR REPLACE INTO transfer_files(transfer_id, file_idx, path, mtime_ns, part_path, done) VALUES (?1,?2,?3,?4,?5,0)",
            params![id, file, path.to_string_lossy(), mtime_ns, part.map(|p| p.to_string_lossy().to_string())],
        )?;
        Ok(())
    }

    pub fn files(&self, id: &str) -> Result<Vec<FileRow>, Error> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT path, mtime_ns, part_path, done FROM transfer_files WHERE transfer_id=?1 ORDER BY file_idx",
        )?;
        let rows = st
            .query_map([id], |r| {
                Ok(FileRow {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    mtime_ns: r.get::<_, Option<i64>>(1)?,
                    part: r.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    done: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn mark_file_done(&self, id: &str, file: u32) -> Result<(), Error> {
        self.c().execute(
            "UPDATE transfer_files SET done=1 WHERE transfer_id=?1 AND file_idx=?2",
            params![id, file],
        )?;
        Ok(())
    }

    pub fn mark_chunk(&self, id: &str, file: u32, idx: u32) -> Result<(), Error> {
        self.c().execute(
            "INSERT OR IGNORE INTO incoming_chunks VALUES (?1,?2,?3)",
            params![id, file, idx],
        )?;
        Ok(())
    }

    pub fn clear_chunks(&self, id: &str, file: u32) -> Result<(), Error> {
        self.c().execute(
            "DELETE FROM incoming_chunks WHERE transfer_id=?1 AND file_idx=?2",
            params![id, file],
        )?;
        Ok(())
    }

    pub fn chunks(&self, id: &str) -> Result<Vec<(u32, u32)>, Error> {
        let c = self.c();
        let mut st = c.prepare("SELECT file_idx, chunk_idx FROM incoming_chunks WHERE transfer_id=?1")?;
        let rows = st
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 再開対象（未完了）の転送
    pub fn active(&self, dir: Direction) -> Result<Vec<Transfer>, Error> {
        let c = self.c();
        let mut st = c.prepare("SELECT json FROM transfers WHERE direction=?1 AND status IN ('active','uploaded')")?;
        let rows = st
            .query_map([if dir == Direction::Outgoing { "out" } else { "in" }], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    pub fn history(&self, limit: u32) -> Result<Vec<HistoryItem>, Error> {
        let rows: Vec<(String, String, String, Option<String>, String, i64)> = {
            let c = self.c();
            let mut st = c.prepare(
                "SELECT transfer_id, direction, status, error, json, updated_at FROM transfers ORDER BY created_at DESC LIMIT ?1",
            )?;
            st.query_map([limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut out = Vec::new();
        for (id, dir, status, error, json, updated_at) in rows {
            let Ok(transfer) = serde_json::from_str(&json) else {
                continue;
            };
            out.push(HistoryItem {
                transfer,
                direction: if dir == "out" {
                    Direction::Outgoing
                } else {
                    Direction::Incoming
                },
                status: LocalStatus::parse(&status),
                error,
                paths: self.files(&id)?.into_iter().map(|f| f.path).collect(),
                updated_at,
            });
        }
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<HistoryItem>, Error> {
        Ok(self.history(10_000)?.into_iter().find(|h| h.transfer.transfer_id == id))
    }
}
