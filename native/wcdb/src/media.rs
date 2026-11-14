//! 媒体定位函数组：图片 hardlink 解析 + 语音数据读取（docs/protocol/04-tables-misc.md）。
//!
//! - 图片：WeFlow 以 hardlinkOnly 流程调 `wcdb_resolve_image_hardlink(md5)` 拿
//!   `{file_name, full_path}`，full_path 必须磁盘存在。md5→file_name 查
//!   hardlink.db 的 `*_hardlink_info_v4`；file_name→磁盘路径经一次 `msg/attach` 递归
//!   建立的"文件名→完整路径"全局索引（按账号目录缓存）解析。
//! - 语音：`wcdb_get_voice_data` 跨 media_*.db 的 `VoiceInfo` 取 silk blob，
//!   按 svr_id / local_id / create_time 命中，经 Name2Id 关联用户名收窄，出参为十六进制字符串。

use crate::ffi::*;
use crate::query::path_to_json_string;
use crate::with_account;
use anyhow::Result;
use rusqlite::{types::ValueRef, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident};

// 逻辑列 → 真实列候选（大小写不敏感）
const HL_MD5: &[&str] = &["md5"];
const HL_FILE_NAME: &[&str] = &["file_name", "fileName", "filename"];
const VOICE_DATA: &[&str] = &["voice_data", "buf", "voicebuf", "voice_buf", "data"];
const VOICE_SVR: &[&str] = &["svr_id", "svrId", "server_id", "svrid", "msg_server_id"];
const VOICE_LOCAL: &[&str] = &["local_id", "localId", "msg_local_id", "local_id_"];
const VOICE_CREATE: &[&str] = &["create_time", "createTime", "create_time_", "time"];
const VOICE_CHAT: &[&str] = &["chat_name_id", "chat_name", "name_id", "chat_id"];
const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];

// ── 附件文件名 → 磁盘完整路径 索引（按账号 msg/attach 根缓存） ──

fn attach_cache() -> &'static Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    // ① 实现方式：函数内 static OnceLock 惰性初始化全局缓存槽，首次调用才建
    //    Mutex<HashMap<缓存键, Arc<索引>>>，之后所有调用共享同一实例（misc.rs cloud_slot 同款手法）。
    // ② 关键程序解析：键为账号 attach 根目录（统一正斜杠 + 小写，见 attach_index），值为
    //    Arc 包裹的索引——命中缓存只需 Arc::clone，读方零拷贝；建索引的高开销全树扫描只付
    //    一次。Mutex 只在写回 insert 时短暂持有，读路径拿锁查表即放。
    // ③ 功能：进程级单例存储，按账号缓存"attach 文件名 → 磁盘完整路径"索引。
}

/// 取得（必要时构建）指定账号 attach 根的文件名索引。文件名小写 → 首个匹配完整路径。
fn attach_index(account: &Account) -> Arc<HashMap<String, PathBuf>> {
    // 路径处理：定位账号的 msg/attach 根；无账号目录直接回空索引（视作"无文件"降级）
    let root = match account.account_dir() {
        Some(d) => d.join("msg").join("attach"),
        None => return Arc::new(HashMap::new()),
    };
    if !root.is_dir() {
        return Arc::new(HashMap::new());
    }
    // 缓存键：路径统一换成正斜杠并小写，屏蔽 Windows 盘符大小写 / 分隔符差异
    let key = root.to_string_lossy().replace('\\', "/").to_lowercase();
    // 读缓存：命中直接 Arc::clone 返回，免重复扫描整棵 attach 树
    if let Ok(g) = attach_cache().lock() {
        if let Some(hit) = g.get(&key) {
            return Arc::clone(hit);
        }
    }
    // 未命中：在锁外全量递归扫描建索引（长任务不持锁），再短暂持锁写回缓存
    let mut map = HashMap::new();
    walk_attach(&root, &mut map);
    let arc = Arc::new(map);
    if let Ok(mut g) = attach_cache().lock() {
        g.insert(key, Arc::clone(&arc));
    }
    arc
    // ① 实现方式：先拼出 msg/attach 根目录并规范化缓存键；查 OnceLock 全局缓存，命中
    //    Arc::clone 即返；未命中则 walk_attach 递归扫描建"小写文件名→完整路径"表后写回。
    // ② 关键程序解析：账号目录缺失 / attach 根不存在都回**空索引**而非报错，调用方按
    //    "文件未找到"降级；建索引在缓存锁外进行、锁只包 insert，避免扫描期间长时间持锁
    //    阻塞其他账号；返回 Arc 让多个查询并发共享同一份只读索引，天然免拷贝。
    // ③ 功能：取得（必要时构建）指定账号 attach 根的文件名索引，是图片 hardlink 解析的
    //    路径数据源。
}

fn walk_attach(dir: &Path, map: &mut HashMap<String, PathBuf>) {
    // 错误处理：目录不可读（权限/损坏）静默返回，不中断整体索引构建
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    // flatten() 跳过读取出错的目录项，其余正常下钻/登记
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = path.is_dir();
        if is_dir {
            walk_attach(&path, map);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // 保留首个命中；同名文件在 attach 树里极少冲突，主图/缩略图后缀不同不会冲突
            map.entry(name.to_ascii_lowercase()).or_insert_with(|| path.clone());
        }
    }
    // ① 实现方式：read_dir 遍历目录项，子目录递归下钻，文件按"小写文件名→完整路径"用
    //    entry().or_insert_with 只登记首个命中。
    // ② 关键程序解析：单目录读取失败不影响其余分支（fail-soft，无 panic 传播）；非 UTF-8
    //    文件名被 and_then(to_str) 静默跳过；小写化是为对齐查询侧 md5/文件名统一小写的约定。
    // ③ 功能：递归扫描 attach 树，构建文件名 → 磁盘完整路径的全局索引。
}

// ── 图片 hardlink 解析 ──

/// 微信 3.x 时代迁移过来的图片按 `<datName>_w.dat`（原图）/ `<datName>_t_w.dat`（缩略图）
/// 落盘，而 hardlink 表里登记的是"文件自身 md5"，消息侧只有 datName/imageMd5（原图 md5），
/// 按 md5 查不到；WeFlow 侧的文件名扫描也只认 `.dat`/`_t`/`_h` 三类，于是整张图判为"未找到"。
/// 这里按文件名变体直接查 attach 索引，顺序 = 质量优先（原图 > 高清 > 迁移原图 > 缩略图）。
const DAT_VARIANT_NAMES: &[&str] = &[
    "{k}.dat",
    "{k}_h.dat",
    "{k}_hd.dat",
    "{k}_w.dat",
    "{k}_b.dat",
    "{k}_c.dat",
    "{k}_t.dat",
    "{k}_t_w.dat",
    "{k}.jpg",
    "{k}.png",
];

/// 由消息图片 md5 解析出 (file_name, full_path)。full_path 保证磁盘存在。
fn resolve_image_dat(account: &Account, md5: &str) -> Option<(String, String)> {
    // 入参规范化：去空白、统一小写；空 md5 直接判未找到
    let md5 = md5.trim().to_ascii_lowercase();
    if md5.is_empty() {
        return None;
    }
    let index = attach_index(account);

    // 1) 经 hardlink.db 各 *_hardlink_info 表：md5 → file_name
    if let Some(hl) = account.layout.hardlink_db.as_ref() {
        // 枚举库内表名，小写包含匹配筛出 hardlink(_info) 系列表（兼容 v3/v4 等版本后缀）
        let tables: Vec<String> = account
            .with_conn(hl, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                let l = t.to_ascii_lowercase();
                l.contains("hardlink_info") || l.contains("hardlink")
            })
            .collect();
        // 逐表查 md5 → file_name → 磁盘路径，任一表命中即返回
        for t in tables {
            if let Some(hit) = lookup_hardlink(account, hl, &t, &md5, &index) {
                return Some(hit);
            }
        }
    }

    // 2) 兜底：以传入的键作为文件名基，按变体后缀在索引里找
    if !is_md5_hex(&md5) {
        return None;
    }
    // 按模板顺序（即质量优先级：原图 > 高清 > 迁移原图 > 缩略图）试各变体文件名
    for tmpl in DAT_VARIANT_NAMES {
        let cand = tmpl.replace("{k}", &md5);
        if let Some(p) = index.get(&cand) {
            return Some((cand, path_to_json_string(p)));
        }
    }
    None
    // ① 实现方式：两级查找——先在 hardlink.db 的各 hardlink_info 表按 md5 查 file_name，
    //    再拿 file_name 到 attach 索引定位磁盘路径；hardlink 链路落空后把 md5 当文件名基，
    //    逐一尝试 DAT_VARIANT_NAMES 变体模板直接命中索引。
    // ② 关键程序解析：表名过滤用小写包含匹配以兼容版本后缀差异；兜底路径先用 is_md5_hex
    //    校验是 32 位十六进制，避免拿任意字符串当文件名乱猜；full_path 取自索引（路径来自
    //    真实目录扫描），天然满足协议"full_path 必须磁盘存在"的硬要求；两级都落空返回 None。
    // ③ 功能：由消息图片 md5 解析 (file_name, full_path)，WeFlow hardlinkOnly 流程的数据源。
}

fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
    // ① 实现方式：长度恰 32 且逐字节校验 ASCII 十六进制字符（0-9/a-f/A-F）。
    // ② 关键程序解析：md5 在消息库里多为 32 位 hex 形态；此校验挡在变体名兜底路径之前，
    //    防止把非 md5 的任意键当文件名基去 attach 索引里乱猜。
    // ③ 功能：判断字符串是否为合法的 md5 hex 形态。
}

/// 在单张 hardlink 表里按 md5 找 file_name，并在 attach 索引里定位完整路径。
fn lookup_hardlink(
    account: &Account,
    hl: &Path,
    table: &str,
    md5: &str,
    index: &HashMap<String, PathBuf>,
) -> Option<(String, String)> {
    // 内省表结构：md5 / file_name 列的真实列名大小写随库而异，用候选序匹配
    let cols = account.columns(hl, table).ok()?;
    let md5_col = find_candidate(&cols, HL_MD5)?;
    let name_col = find_candidate(&cols, HL_FILE_NAME)?;
    // SQL 拼接：表名/列名无法参数化，必须过 sanitize_ident 白名单后才进 format!
    let safe_table = sanitize_ident(table).ok()?;
    let sql = format!(
        "SELECT \"{name}\" FROM \"{tbl}\" WHERE lower(\"{m}\") = ?1",
        name = sanitize_ident(&name_col).ok()?,
        tbl = safe_table,
        m = sanitize_ident(&md5_col).ok()?,
    );
    // 参数化执行：?1 绑定 md5，flatten() 丢弃类型不符的行，收齐所有同名 file_name
    let names: Vec<String> = account
        .with_conn(hl, |c| -> Result<Vec<String>> {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([md5], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().collect())
        })
        .ok()?;
    // file_name 依次（小写化）到 attach 索引定位，首个命中即返回
    for name in names {
        let key = name.to_ascii_lowercase();
        if let Some(p) = index.get(&key) {
            return Some((name, path_to_json_string(p)));
        }
    }
    None
    // ① 实现方式：先 table_columns + find_candidate 内省出 md5/file_name 真实列名，拼
    //    SELECT file_name WHERE lower(md5)=?1 参数化查询，再把查到的 file_name 逐个去
    //    attach 索引定位完整路径。
    // ② 关键程序解析：WHERE 里对列套 lower() 是因部分库 md5 存大写，绑定侧已在调用链
    //    统一小写；表名/列名均过 sanitize_ident（注入防线）后才允许拼接；查询侧任何一步
    //    失败（列缺失/表损坏/连接失败）都 ok()? 降级为 None，交上层走变体名兜底。
    // ③ 功能：在单张 hardlink 表按 md5 找 file_name，并解析出磁盘完整路径。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_resolve_image_hardlink(
    handle: i64,
    md5: *const c_char,
    _account_dir: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let md5 = cstr(md5);
        // 查找逻辑收进闭包：命中产出 {file_name, full_path, md5} JSON，未命中 Err 带原文案
        let result = (|| -> Result<String> {
            match resolve_image_dat(account, md5) {
                Some((file_name, full_path)) => Ok(json!({
                    "file_name": file_name,
                    "full_path": full_path,
                    "md5": md5.trim().to_ascii_lowercase(),
                })
                .to_string()),
                None => Err(anyhow::anyhow!("未找到图片 hardlink: {md5}")),
            }
        })();
        // 未命中时返回非 0，交由上层记录 miss / 回退
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：with_account 按 handle 路由账号（含 panic 防护），cstr 借读入参；
        //    闭包调 resolve_image_dat 组装 JSON，emit_json 统一写出参并把 Err 映射为
        //    ERR_GENERIC 返回码（错误全文进环形日志）。
        // ② 关键程序解析：出参 out_json 由 emit_json→write_out→alloc_out 分配，TS 侧须
        //    经 wcdb_free_string 归还；md5 回传前统一 trim+小写，与库内存量口径一致；
        //    full_path 已由 resolve_image_dat 保证磁盘存在（hardlinkOnly 流程硬要求）。
        // ③ 功能：FFI 入口——按 md5 解析单张图片的 file_name 与完整磁盘路径。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_resolve_image_hardlink_batch(
    handle: i64,
    requests_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let reqs = cstr(requests_json);
        let result = (|| -> Result<String> {
            // 入参容错：空串按 [] 处理；非数组/畸形 JSON 一律回落空数组，保证出参合法
            let parsed: Value = serde_json::from_str(if reqs.trim().is_empty() { "[]" } else { reqs })
                .unwrap_or(Value::Array(Vec::new()));
            let arr = parsed.as_array().cloned().unwrap_or_default();
            let mut rows = Vec::with_capacity(arr.len());
            // 逐条解析 md5 并查询；每条请求独立产出带 index 的结果，互不牵连
            for (i, r) in arr.iter().enumerate() {
                let md5 = r["md5"].as_str().unwrap_or("").trim().to_ascii_lowercase();
                // md5 缺失/为空：单独记错误项并跳过查询，不算整体失败
                if md5.is_empty() {
                    rows.push(json!({ "index": i, "md5": "", "success": false, "error": "md5 为空" }));
                    continue;
                }
                // 命中带 data{file_name, full_path}，未命中带 error 文案（fail-soft）
                match resolve_image_dat(account, &md5) {
                    Some((file_name, full_path)) => rows.push(json!({
                        "index": i,
                        "md5": md5,
                        "success": true,
                        "data": { "file_name": file_name, "full_path": full_path },
                    })),
                    None => rows.push(json!({
                        "index": i, "md5": md5, "success": false, "error": "未找到图片文件"
                    })),
                }
            }
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：解析 requests_json 为数组后逐条调 resolve_image_dat，按入参顺序
        //    组装 {index, md5, success, data|error} 结果数组，整体 emit_json 返回。
        // ② 关键程序解析：单条失败不影响其他条目，错误以 success:false + error 文案内联
        //    表达而非整体 rc 失败，TS 可按 index 精确归位；解析容错链（空串→"[]"、畸形→
        //    空数组、非数组对象→unwrap_or_default）保证任何畸形入参都产出合法 JSON。
        // ③ 功能：FFI 入口——批量解析多张图片 hardlink，单条隔离失败。
    })
}

// ── 语音数据读取 ──

/// 从 Name2Id 解析候选用户名对应的 rowid 集合。
fn name2id_rowids(conn: &Connection, table: &str, names: &[String]) -> Vec<i64> {
    // 空候选集无需查询，直接空集（后续收窄谓词会自然关闭）
    if names.is_empty() {
        return Vec::new();
    }
    // 内省 Name2Id 表结构，定位用户名列（列名大小写随库而异）
    let cols = match table_columns(conn, table) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let name_col = match find_candidate(&cols, N2I_NAME) {
        Some(c) => c,
        None => return Vec::new(),
    };
    // SQL 拼接：表名/列名过 sanitize_ident 白名单，防止注入
    let safe = match sanitize_ident(&name_col) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let safe_table = match sanitize_ident(table) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // IN 列表占位符按候选数展开，全部走 ? 绑定参数
    let placeholders = vec!["?"; names.len()].join(",");
    let sql = format!("SELECT rowid FROM \"{safe_table}\" WHERE \"{safe}\" IN ({placeholders})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    // 执行：把候选名逐个装箱成 ToSql 引用绑定；flatten() 丢弃类型不符的行
    let refs: Vec<&dyn rusqlite::ToSql> =
        names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
    let out: Vec<i64> = match stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    };
    out
    // ① 实现方式：table_columns + find_candidate 内省出用户名列，拼 SELECT rowid WHERE
    //    name IN (?,?,...) 的参数化查询，返回候选用户名对应的 rowid 集合。
    // ② 关键程序解析：所有失败路径（表损坏/列缺失/标识符非法/prepare 失败）都收敛为
    //    空 Vec 而非报错，让调用方"收窄失败=不收窄"地降级；IN 值全部 ? 绑定，只有表名/
    //    列名经白名单拼接；rowid 用于后续拼 chat_name_id IN (...) 的收窄谓词。
    // ③ 功能：从 Name2Id 表解析候选用户名对应的 rowid 集合（语音收窄查询的中间产物）。
}

/// 读取语音 silk blob 的 hex。命中不到返回 None。
fn find_voice_hex(
    account: &Account,
    session_id: &str,
    create_time: i64,
    local_id: i64,
    svr_id: i64,
    candidates: &[String],
) -> Option<String> {
    // 候选用户名清洗：去空白、滤空；session_id 并入候选并去重排序
    let mut names: Vec<String> = candidates
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let sid = session_id.trim().to_string();
    if !sid.is_empty() && !names.iter().any(|n| n == &sid) {
        names.push(sid);
    }
    names.sort();
    names.dedup();

    // 遍历所有 media_*.db，按表名含 voice 筛出 VoiceInfo 系列表
    for db in &account.layout.media_dbs {
        let tables: Vec<String> = account
            .with_conn(db, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.to_ascii_lowercase().contains("voice"))
            .collect();
        // 逐库逐表查询，任一命中即返回（跨 media_*.db 的分库容灾）
        for vt in tables {
            if let Some(hexv) = voice_in_table(account, db, &vt, &names, create_time, local_id, svr_id) {
                return Some(hexv);
            }
        }
    }
    None
    // ① 实现方式：清洗并去重候选用户名后，遍历 account.layout.media_dbs 里每张 voice
    //    系表，调 voice_in_table 按 svr_id/local_id/create_time 命中 silk blob 取 hex。
    // ② 关键程序解析：session_id 兜底并入候选——某些消息候选列缺失时仍能靠会话方 id 收窄；
    //    sort+dedup 保证 IN 列表稳定且不重复绑定；库/表双循环 + 首中即返，微信分库存储的
    //    语音无论落在哪个 media_*.db 都能找到；全部落空返回 None 交上层兜底。
    // ③ 功能：跨 media_*.db 的 VoiceInfo 表检索语音 silk blob，返回 hex 字符串。
}

#[allow(clippy::too_many_arguments)]
fn voice_in_table(
    account: &Account,
    db: &Path,
    vtable: &str,
    names: &[String],
    create_time: i64,
    local_id: i64,
    svr_id: i64,
) -> Option<String> {
    // 内省表结构：语音 blob / svr_id / local_id / create_time / chat_name_id 各列真实名
    let cols = account.columns(db, vtable).ok()?;
    let data_col = find_candidate(&cols, VOICE_DATA)?;
    let svr_col = find_candidate(&cols, VOICE_SVR);
    let local_col = find_candidate(&cols, VOICE_LOCAL);
    let create_col = find_candidate(&cols, VOICE_CREATE);
    let chat_col = find_candidate(&cols, VOICE_CHAT);
    // 表名/数据列必过白名单；定位/收窄列可选，拼 SQL 前再逐个 sanitize
    let safe_data = sanitize_ident(&data_col).ok()?;
    let safe_table = sanitize_ident(vtable).ok()?;

    // 定位谓词：svr_id > local_id > create_time（单一绑定参数用 ?1）
    let (id_where, id_param): (Option<String>, Option<i64>) = match (&svr_col, &local_col, &create_col) {
        // 三级降序择优：列存在且对应 id 非 0 才采用；谓词值全部 ?1 绑定，不拼接
        (Some(sc), _, _) if svr_id != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(sc).ok()?)), Some(svr_id)),
        (_, Some(lc), _) if local_id != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(lc).ok()?)), Some(local_id)),
        (_, _, Some(cc)) if create_time != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(cc).ok()?)), Some(create_time)),
        _ => (None, None),
    };
    // 三种定位键都不可用（列缺失或值全 0）时无从定位，直接放弃本表
    if id_where.is_none() {
        return None;
    }

    // 取库连接：conn() 拿共享连接句柄，lock() 独占期间完成整轮查询
    let conn = account.conn(db).ok()?;
    let conn = conn.lock().ok()?;

    // 收窄：优先带 chat_name_id 过滤，命中不到再仅用 id 谓词
    let rowids: Vec<i64> = if let Some(cc) = &chat_col {
        // 本库内找 Name2Id 表，把候选用户名折成 rowid 集合；失败则放弃收窄
        let n2i = find_name2id(&cols_of_tables(&conn))?;
        let _ = cc;
        name2id_rowids(&conn, &n2i, names)
    } else {
        Vec::new()
    };

    // 单轮查询闭包：with_chat 决定是否叠加上述收窄谓词
    let run = |with_chat: bool| -> Option<String> {
        let base = id_where.clone()?;
        // 拼接 WHERE：收窄模式下 rowid 集合转 IN 列表（i64 数值无注入面）叠加 id 谓词
        let (where_sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if with_chat && !rowids.is_empty() {
            let inlist = rowids.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",");
            let cc = chat_col.as_ref().and_then(|c| sanitize_ident(c).ok())?;
            (format!("\"{cc}\" IN ({inlist}) AND {base}"), vec![Box::new(id_param.unwrap_or(0))])
        } else {
            // 仅 id 谓词的降级形态
            (base, vec![Box::new(id_param.unwrap_or(0))])
        };
        // hex() 让 SQLite 在库端把 blob 转小写十六进制串，LIMIT 1 只要一条
        let sql = format!("SELECT hex(\"{safe_data}\") FROM \"{safe_table}\" WHERE {where_sql} LIMIT 1");
        let mut stmt = conn.prepare(&sql).ok()?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p.as_ref() as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(refs.as_slice()).ok()?;
        // 取首行：Text 说明库里已存 hex 串，Blob 则 Rust 侧现算 hex；统一小写
        match rows.next().ok()? {
            Some(row) => match row.get_ref(0).ok()? {
                ValueRef::Text(t) => Some(String::from_utf8_lossy(t).to_ascii_lowercase()),
                ValueRef::Blob(b) => Some(hex::encode(b).to_ascii_lowercase()),
                _ => None,
            },
            None => None,
        }
    };

    // 两段式执行：先带收窄（精确），空结果再退纯 id 谓词（宽松），任一命中即返回
    run(true).or_else(|| run(false))
    // ① 实现方式：内省各列 → 按 svr_id > local_id > create_time 三级优先挑定位谓词（?1
    //    绑定）→ 先用 Name2Id 把候选用户名折成 rowid 集合收窄，再以"收窄/纯 id"两种 WHERE
    //    各跑一轮 SELECT hex(voice_data) ... LIMIT 1，前轮空则后轮兜底。
    // ② 关键程序解析：hex() 下推到 SQLite 避免把整段 silk blob 拷回 Rust；Text/Blob 两种
    //    存储形态都兼容；rowid IN 列表是 i64 数值拼接无注入面，列名仍全走 sanitize_ident；
    //    收窄失败（无 Name2Id/无 chat 列）不致命，仅退化为纯 id 谓词查询。
    // ③ 功能：在单张 voice 系表内按消息定位键取语音 blob 的小写 hex 串。
}

fn cols_of_tables(conn: &Connection) -> Vec<String> {
    list_tables(conn).unwrap_or_default()
    // ① 实现方式：直接转调 list_tables，连接损坏/查询失败时 unwrap_or_default 回空表。
    // ② 关键程序解析：仅是 find_name2id 的入参整形——空表会让 Name2Id 定位失败，
    //    上层据此放弃收窄、退化为纯 id 谓词查询，属预期降级而非错误。
    // ③ 功能：取连接内全部表名（语音收窄时用于定位 Name2Id 表）。
}

/// 在库中定位 Name2Id 表（名字含 name2id）。
fn find_name2id(tables: &[String]) -> Option<String> {
    tables
        .iter()
        // 优先精确同名（忽略大小写），落空再退含 name2id 字样的包含匹配
        .find(|t| t.eq_ignore_ascii_case("Name2Id"))
        .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))
        .cloned()
    // ① 实现方式：两轮查找——eq_ignore_ascii_case 精确匹配优先，or_else 兜底小写包含匹配。
    // ② 关键程序解析：与 locate_table（contacts.rs）同思路；精确优先避免同名前缀误命中，
    //    如 Name2Id_old 不会被第一轮选中；全部落空返回 None，调用方放弃收窄降级查询。
    // ③ 功能：在表名列表中定位 Name2Id 表（用户名→rowid 映射）。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_voice_data(
    handle: i64,
    session_id: *const c_char,
    create_time: i32,
    local_id: i32,
    svr_id: i64,
    candidates_json: *const c_char,
    out_hex: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let cands_json = cstr(candidates_json);
        // 候选用户名列表容错：空串按 [] 处理，畸形 JSON 回空候选（find_voice_hex 自会并入 sid）
        let candidates: Vec<String> = serde_json::from_str(if cands_json.trim().is_empty() { "[]" } else { cands_json })
            .unwrap_or_default();
        // 负数时间/编号无意义，max(0) 归零后再转 i64 交给查找
        let hexv = find_voice_hex(account, sid, create_time.max(0) as i64, local_id.max(0) as i64, svr_id, &candidates);
        match hexv {
            Some(hex) => {
                write_out(out_hex, &hex);
                OK
            }
            None => {
                // 未命中：给出空串（TS 端 `hex || undefined`），但仍回 0 让其进入 exec_query 兜底前先不报错
                write_out(out_hex, "");
                ERR_GENERIC
            }
        }
        // ① 实现方式：with_account 路由账号，cstr 借读两个字符串入参；解析候选列表后调
        //    find_voice_hex 跨 media_*.db 检索，命中 write_out 写 hex 返 OK，未命中写空串
        //    返 ERR_GENERIC。
        // ② 关键程序解析：出参无论成败都写（未命中写空串而非留空），保证 TS 侧永远有合法
        //    字符串可读；hex 已是小写（库端 hex() 或 Rust 侧 hex::encode 统一小写）；错误码
        //    仅作 miss 信号，详细原因看环形日志。
        // ③ 功能：FFI 入口——按 session_id/时间/编号取单条语音 silk blob 的 hex 字符串。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_voice_data_batch(
    handle: i64,
    requests_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let reqs = cstr(requests_json);
        let result = (|| -> Result<String> {
            // 入参容错：空串按 []、畸形/非数组 JSON 回空数组，保证任何输入都出合法 JSON
            let parsed: Value = serde_json::from_str(if reqs.trim().is_empty() { "[]" } else { reqs })
                .unwrap_or(Value::Array(Vec::new()));
            let arr = parsed.as_array().cloned().unwrap_or_default();
            let mut rows = Vec::with_capacity(arr.len());
            // 逐条取定位键并检索；字段缺失一律 unwrap_or(0)/空串，单条异常不拖垮整批
            for (i, r) in arr.iter().enumerate() {
                let sid = r["session_id"].as_str().unwrap_or("").to_string();
                let ct = r["create_time"].as_i64().unwrap_or(0);
                let lid = r["local_id"].as_i64().unwrap_or(0);
                // svr_id 兼容数字/字符串两种形态（TS 侧序列化口径不一），字符串 parse 失败按 0
                let svr = r["svr_id"].as_i64().or_else(|| r["svr_id"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
                // 候选用户名只收字符串元素，其余静默丢弃
                let cands: Vec<String> = r["candidates"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let hexv = find_voice_hex(account, &sid, ct, lid, svr, &cands);
                // 命中带 hex 键，未命中仅 {index}（TS 侧按缺键视为无语音）
                let mut obj = json!({ "index": i });
                if let Some(h) = hexv {
                    obj["hex"] = Value::String(h);
                }
                rows.push(obj);
            }
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：解析 requests_json 为数组后逐条抽 session_id/create_time/local_id/
        //    svr_id/candidates，调 find_voice_hex 检索，按 index 顺序组装 [{index, hex?}]
        //    结果数组，整体 emit_json 返回。
        // ② 关键程序解析：单条失败不内联 error 文案，仅省略 hex 键表示 miss——与单条版
        //    （空串+非 0）不同，批量的 miss 由"键缺失"表达，消费方按 hasOwnProperty 判定；
        //    svr_id 双形态解析是兼容 TS 端数字/字符串序列化差异的历史约定。
        // ③ 功能：FFI 入口——批量取多条语音 silk blob 的 hex，单条隔离失败。
    })
}
