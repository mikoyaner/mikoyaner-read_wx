//! 只读端到端核验：从共享密钥库解析各库密钥，打开账号并验证 message/contact 等
//! 分库都可读、能取到消息行。**密钥来自密钥库，不从命令行传入**，故调用者无需接触明文密钥。
//!
//! 用法（需先 `key-provider bulk` 导入密钥库）：
//!   cargo run -q -p weflow-core --example verify -- <session.db 绝对路径>

use anyhow::Result;
use std::path::PathBuf;
use weflow_core::db::{derive_account_dir, Account};
use weflow_core::keystore::KeyStore;
use weflow_core::messages::session_message_rows;
use serde_json::Value;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: verify <session.db 绝对路径>");
        std::process::exit(2);
    }
    let session_db = PathBuf::from(&args[0]);
    if !session_db.is_file() {
        eprintln!("session.db 不存在: {}", session_db.display());
        std::process::exit(1);
        // 两道入口校验：缺参退出码 2、文件不存在退出码 1，都不进入后续只读核验。
    }

    // 从密钥库解析 session key（不接触明文：仅程序内部使用）。
    let store = KeyStore::load_default();
    let adir = derive_account_dir(&session_db);
    let session_key = store.resolve_key(&session_db, adir.as_deref(), "");
    if session_key.is_empty() {
        eprintln!("密钥库中没有该账号 session.db 的密钥；请先 `key-provider bulk --account-dir <dir> --file <scan.txt>`");
        std::process::exit(1);
        // 密钥解析不到就直接终止并给出补救命令，绝不回退到命令行传明文密钥。
    }

    println!("── 打开账号（session 主 key 由密钥库解析）──");
    let account = Account::open(&session_db.to_string_lossy(), &session_key)?;
    // Account::open 完成布局发现与主库连接；这一步通过说明 session.db 主密钥正确。

    // 1) 逐个打开所有已知分库：能开即证明每库密钥解析正确。
    let mut opened = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let try_open = |label: &str, p: &PathBuf, opened: &mut usize, failed: &mut Vec<String>, account: &Account| {
        match account.conn(p) {
            Ok(_) => *opened += 1,
            Err(e) => failed.push(format!("{label}: {e}")),
        }
    };
    // 闭包只做"能开/不能开"的判定：成功计数，失败把库名+原因记入 failed，不打断流程。
    for (i, db) in account.layout.message_dbs.iter().enumerate() {
        try_open(&format!("message_{i}"), db, &mut opened, &mut failed, &account);
        // 逐个 message_N.db：每库密钥不同，能开即证明逐库密钥解析都正确。
    }
    for (i, db) in account.layout.media_dbs.iter().enumerate() {
        try_open(&format!("media_{i}"), db, &mut opened, &mut failed, &account);
        // media_N.db 同样逐库尝试，覆盖媒体分库的独立密钥。
    }
    for (label, opt) in [
        ("contact", &account.layout.contact_db),
        ("sns", &account.layout.sns_db),
        ("emoticon", &account.layout.emoticon_db),
        ("hardlink", &account.layout.hardlink_db),
    ] {
        if let Some(p) = opt {
            try_open(label, p, &mut opened, &mut failed, &account);
        }
    }
    // 四个可选单库（contact/sns/emoticon/hardlink）存在才尝试，缺失不算失败。
    println!("分库打开成功: {opened} 个；失败: {} 个", failed.len());
    for f in &failed {
        println!("  ✗ {f}");
    }
    // 失败清单逐条打印：一眼看出是哪个库的密钥/文件有问题。

    // 2) 会话数（session.db 可读的旁证）。
    let session_count: i64 = account.with_conn(&account.layout.session_db, |c| {
        Ok(c.query_row("SELECT count(*) FROM SessionTable", [], |r| r.get::<_, i64>(0))?)
    })?;
    println!("SessionTable 会话数: {session_count}");
    // 能对 SessionTable 做 count(*) 就证明 session.db 解密可读，作为会话库的旁证。

    // 3) 取最近会话，读取其消息行（证明按每库密钥路由到 message_N.db 并解密成功）。
    let recent: Vec<String> = account.with_conn(&account.layout.session_db, |c| {
        let mut stmt = c.prepare("SELECT username FROM SessionTable ORDER BY sort_timestamp DESC LIMIT 25")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        Ok(v)
    })?;
    // 按 sort_timestamp 倒序取最近 25 个会话的 username，作为消息读取的取样集合。

    println!("── 最近会话消息读取（不打印聊天内容，仅统计）──");
    let mut sessions_with_msgs = 0usize;
    let mut total_msgs = 0usize;
    let mut reported = 0usize;
    for sid in &recent {
        // session_message_rows 内部按每库密钥路由 message_N.db 并解密；这里只消费结果做统计。
        match session_message_rows(&account, sid, 0, i64::MAX, true) {
            Ok(rows) => {
                if rows.is_empty() {
                    continue;
                    // 空会话不计入统计，直接看下一个。
                }
                sessions_with_msgs += 1;
                total_msgs += rows.len();
                // 非空会话累计两个口径：含消息的会话数、消息总条数。
                if reported < 8 {
                    // 只对前 8 个会话做详细报告，其余只计入总数，避免刷屏。
                    reported += 1;
                    let mut type_count: std::collections::BTreeMap<i64, usize> = Default::default();
                    for row in &rows {
                        if let Value::Object(m) = row {
                            if let Some(v) = m.get("local_type").and_then(|x| x.as_i64()) {
                                *type_count.entry(v).or_default() += 1;
                            }
                        }
                    }
                    println!("  会话[{} 字] 消息 {} 条，type 分布 {:?}", sid.len(), rows.len(), type_count);
                    // 统计 local_type 分布而不打印聊天内容：验证"能解密"的同时不泄露明文。
                }
            }
            Err(e) => println!("  会话读取失败: {e}"),
            // 单会话失败不中断整体核验，记录后继续下一个。
        }
    }
    println!("取样 {}/{} 个最近会话，其中 {} 个含消息，共 {} 条", recent.len(), recent.len(), sessions_with_msgs, total_msgs);
    // 汇总取样结果：这些数字就是"真实数据可读"的直接证据。

    if failed.is_empty() && opened > 0 && sessions_with_msgs > 0 {
        println!("\n✓ 端到端通过：每库独立密钥解析 + 分库可读 + 真实消息行可读。");
        Ok(())
    } else {
        println!("\n结果存在失败项或未能读出任何消息，见上。");
        std::process::exit(1);
        // 收官判定：分库全开 + 至少读出消息才算端到端通过，否则以退出码 1 结束。
    }
}
