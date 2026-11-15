//! 只读调试：用给定密钥打开一个 .db，打印表清单/列/或执行一条 SELECT。
//! 仅用于本机自有的微信数据核验（任务 #7）。用法：
//!   cargo run -p weflow-core --example inspect -- <db_path> <64hex_key> [sql]

use rusqlite::Connection;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: inspect <db_path> <64hex_key> [sql]");
        std::process::exit(2);
        // 参数不足即以退出码 2 终止：<db_path> <64hex_key> [sql] 三段式用法。
    }
    let path = &args[0];
    let key_arg = args[1].trim();
    let key = if key_arg.is_empty() || key_arg.eq_ignore_ascii_case("store") {
        // 从共享密钥库按路径解析，调用者无需在命令行给出明文密钥。
        let db = std::path::Path::new(path);
        let adir = weflow_core::db::derive_account_dir(db);
        weflow_core::keystore::KeyStore::load_default().resolve_key(db, adir.as_deref(), "")
    } else {
        key_arg.to_string()
        // 命令行直接给了密钥：原样采用（上面 'store' 分支才走密钥库解析）。
    };
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!("key must be 64 hex (or pass 'store' to resolve from keystore)");
        std::process::exit(2);
        // 密钥必须是 64 位 hex：SQLCipher 的 PRAGMA key 只认 x'<64hex>' 这种格式。
    }
    let conn = Connection::open(path).expect("open");
    conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";")).expect("pragma");
    // 打开库文件并注入密钥初始化解密上下文；密钥对不对要到真正查询时才见分晓。

    if let Some(sql) = args.get(2) {
        // 带第三个参数 = 执行调用者给的 SELECT，这是只读调试的主路径。
        let mut stmt = conn.prepare(sql).expect("prepare");
        let ncols = stmt.column_count();
        let names: Vec<String> = (0..ncols).map(|i| stmt.column_name(i).unwrap_or("?").to_string()).collect();
        println!("columns: {names:?}");
        // 先打印列名清单，再看数据行，对照着读结果。
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0usize;
        while let Some(r) = rows.next().expect("row") {
            // 逐行迭代：每行的每列按实际类型转成可读字符串后拼成一行打印。
            let vals: Vec<String> = (0..ncols)
                .map(|i| {
                    r.get_ref(i)
                        .map(|v| match v {
                            rusqlite::types::ValueRef::Null => "NULL".into(),
                            rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                            rusqlite::types::ValueRef::Real(x) => x.to_string(),
                            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                            rusqlite::types::ValueRef::Blob(b) => format!("<blob {}B>", b.len()),
                            // 五种值类型各转成可读字符串：BLOB 只标字节数，不把二进制刷到终端。
                        })
                        .unwrap_or_else(|_| "?".into())
                })
                .collect();
            println!("[{n}] {vals:?}");
            n += 1;
            if n >= 200 {
                println!("... (truncated at 200 rows)");
                break;
                // 最多 200 行就截断：探针不是导出工具，防止大表刷屏。
            }
        }
        println!("total printed: {n}");
        return;
    }

    // 默认：列全部表 + 每张表列名
    let mut stmt = conn
        .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")
        .expect("master");
    let objs: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("collect");
    println!("== {} objects in {}", objs.len(), path);
    // sqlite_master 是 SQLite 系统目录：查它可列出全部表/视图，作为库结构总览。
    for (name, ty) in &objs {
        let mut ps = conn.prepare(&format!("PRAGMA table_info(\"{name}\")")).expect("ti");
        let cols: Vec<String> = ps
            .query_map([], |r| r.get::<_, String>(1))
            .expect("cq")
            .collect::<Result<_, _>>()
            .expect("cc");
        println!("[{ty}] {name}: {cols:?}");
        // 每张表用 PRAGMA table_info 抽列名，打印「[类型] 表名: [列...]」的结构清单。
    }
}
