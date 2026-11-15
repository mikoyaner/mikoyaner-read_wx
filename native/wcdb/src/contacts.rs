//! 联系人 / 群聊 / 头像 / 表情 CDN 函数组（docs/protocol/03、05）。
//!
//! 覆盖必需符号：wcdb_get_contact、wcdb_get_display_names、wcdb_get_avatar_urls、
//! wcdb_get_group_member_count、wcdb_get_group_members、wcdb_get_emoticon_cdn_url，
//! 及可选的 wcdb_get_group_member_counts。
//!
//! `contact` 表名与列语义由 wcdbCore.ts 的 darwin SQL 路径确证（表名单数 `contact`，
//! 列 username/remark/nick_name/alias/big_head_img_url/...）。群成员与表情库表结构
//! 未经 dbKey 校准，故对这两者采取"运行时内省 + 候选表/列名"策略，未命中时返回合法空
//! 结果（`[]`/count=0/空串）而非报错，确保 initialize 不因 schema 差异失败。

use crate::ffi::*;
use crate::query::query_rows_json;
use crate::{push_log, with_account};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident};

// ── 通用内省辅助 ──

fn ci_get<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    // ① 实现方式：线性扫描键集，eq_ignore_ascii_case 忽略大小写取首个命中键的值。
    // ② 关键程序解析：serde_json::Map 默认不保序，这里不依赖键序，仅保证"存在即找到"；
    //    大小写不敏感是因为 contact 库列名在 darwin/win 口径间有 camelCase/snake_case 差异，
    //    行对象经 query_rows_json 产出后键名大小写不可预知。
    // ③ 功能：在 JSON 行对象里做大小写不敏感的键查找（contact 行取列的统一入口）。
}

fn pick_string(map: &Map<String, Value>, candidates: &[&str]) -> Option<String> {
    // 按候选序取值：先命中先返回（候选序优先于列序）
    for c in candidates {
        if let Some(v) = ci_get(map, c) {
            // 标量统一转字符串；对象/数组等非标量跳过继续试下一候选
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            // 全空白串视作"该列无值"，不拦截候选链
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
    // ① 实现方式：外层按 candidates 顺序、内层经 ci_get 大小写不敏感取值，非标量转字符串，
    //    首个非空白值即返回。
    // ② 关键程序解析：Number/Bool 也接受——联系人表里 local_type/flag 常以整型存储，统一
    //    字符串化让上层比较口径一致；空白串不命中是防止库里有 "" 占位列屏蔽了真实列。
    // ③ 功能：按候选序从 contact 行对象提取首个可用字符串值（remark/nick/avatar 等都用它）。
}

/// 在库中按名字提示定位表：优先精确匹配，其次包含匹配。
fn locate_table(conn: &Connection, hints: &[&str]) -> Option<String> {
    // 库打不开/读不出表名时直接失败，交上层报错或降级
    let tables = list_tables(conn).ok()?;
    // 第一轮：全部提示词按忽略大小写的精确匹配（最可信）
    for h in hints {
        if let Some(t) = tables.iter().find(|t| t.eq_ignore_ascii_case(h)) {
            return Some(t.clone());
        }
    }
    // 第二轮：精确落空再退含提示词的小写包含匹配
    for h in hints {
        if let Some(t) = tables.iter().find(|t| t.to_ascii_lowercase().contains(*h)) {
            return Some(t.clone());
        }
    }
    None
    // ① 实现方式：list_tables 枚举后两轮查找——先逐提示词精确匹配，再逐提示词包含匹配，
    //    首个命中返回库内真实表名。
    // ② 关键程序解析：群成员/表情库表结构未经 dbKey 校准，只能靠名字提示定位；精确优先
    //    避免前缀相近表（如 emotion vs emoticon）误命中；返回原始大小写以保 SQL 拼接一致。
    // ③ 功能：运行时内省定位候选表名的通用实现（群成员、表情 CDN 都经它找表）。
}

fn contact_db_path(account: &Account) -> Option<PathBuf> {
    // 首选 layout 登记的 contact 库，且要求磁盘确实存在
    if let Some(p) = &account.layout.contact_db {
        if p.is_file() {
            return Some(p.clone());
        }
    }
    // 回退：账号目录/db_storage/contact/contact.db
    let dir = account.account_dir()?;
    let p = dir.join("db_storage").join("contact").join("contact.db");
    p.is_file().then_some(p)
    // ① 实现方式：先取账号布局里登记的 contact_db 路径并 is_file 校验，失效则按微信
    //    默认目录约定拼 db_storage/contact/contact.db 再验存在性。
    // ② 关键程序解析：两处都强制 is_file——路径登记了但文件缺失（账号未同步完）与路径
    //    不存在同等对待，返回 None 让上层 bail/降级，绝不带不存在的库路径去开连接。
    // ③ 功能：解析联系人库的真实磁盘路径（contact 组所有导出的数据源定位）。
}

/// 批量取 contact 行（按 username IN (...)），返回 Vec<原始行对象>。
fn fetch_contacts(account: &Account, db: &Path, usernames: &[String]) -> Result<Vec<Value>> {
    // 内省定位联系人表（表名单数 contact，但旧库有 contacts/friend 变体），未找到即报错
    let table = account.with_conn(db, |c| {
        locate_table(c, &["contact", "contacts", "friend", "Contact"])
            .ok_or_else(|| anyhow::anyhow!("contact 库中未找到联系人表"))
    })?;
    // 表名过白名单后才允许拼接
    let safe_table = sanitize_ident(&table)?;
    account.with_conn(db, |conn| {
        // 空名单：全表捞（供联系人列表类调用方使用）
        if usernames.is_empty() {
            let sql = format!("SELECT * FROM \"{safe_table}\"");
            return query_rows_json(conn, &sql);
        }
        // 非空名单：内省出 username 真实列名，缺失视为 schema 异常
        let cols = table_columns(conn, &table)?;
        let Some(name_col) = find_candidate(
            &cols,
            &["username", "user_name", "userName", "usrName", "str_username", "name"],
        ) else {
            bail!("contact 表无 username 列");
        };
        // IN 列表按名单长度展开占位符；值全部 ? 绑定，仅表名/列名经白名单拼接
        let safe_col = sanitize_ident(&name_col)?;
        let placeholders = std::iter::repeat("?").take(usernames.len()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT * FROM \"{safe_table}\" WHERE \"{safe_col}\" IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            usernames.iter().map(|u| u as &dyn rusqlite::ToSql).collect();
        crate::query::statement_rows_json(&mut stmt, refs.as_slice())
    })
    // ① 实现方式：locate_table 内省定位联系人表 → 按名单是否为空走"全表 SELECT *"或
    //    "username IN (?,?,...)"参数化查询 → statement_rows_json 把行转 JSON 对象数组。
    // ② 关键程序解析：SELECT * 保留原始列名（camelCase/snake_case 差异留给 pick_string
    //    大小写不敏感消化）；错误链全程 anyhow bail，由调用方决定报错还是降级；两段
    //    with_conn 各自短借连接，不长期持锁。
    // ③ 功能：按 username 名单批量取 contact 行（原始行对象数组），contact 组的公共数据层。
}

// 显示名候选链：备注 > 昵称 > 别名（snake_case/camelCase 双口径并存，pick_string 按序取首个命中）
const CONTACT_DISPLAY: &[&str] = &[
    "remark", "Remark", "nick_name", "nickName", "nickname", "NickName", "alias", "Alias",
];
// 昵称列候选（补别名单用：nickName camelCase 是 TS 格式化器的硬要求键）
const CONTACT_NICK: &[&str] = &["nick_name", "nickName", "nickname", "NickName"];
// 头像列候选：大图 > 小图 > 通用头图，大图优先保证展示清晰度
const CONTACT_AVATAR: &[&str] = &[
    "big_head_img_url", "bigHeadImgUrl", "big_head_url", "bigHeadUrl",
    "small_head_img_url", "smallHeadImgUrl", "small_head_url", "smallHeadUrl",
    "head_img_url", "headImgUrl", "avatar_url", "avatarUrl",
];

/// 给 contact 行补齐协议要求的推荐键（nickName camelCase 为硬要求；local_type/flag/quan_pin snake_case）。
fn add_contact_aliases(map: &mut Map<String, Value>) {
    // username 是全表对齐的主键，先解析出来供各补键兜底
    let username = pick_string(map, &["username", "user_name", "userName", "usrName"]);
    if let Some(u) = &username {
        map.entry("username").or_insert_with(|| Value::String(u.clone()));
    }
    // alias/remark：or_insert 只补缺，不覆盖库内已有值
    if let Some(alias) = pick_string(map, &["alias", "Alias"]) {
        map.entry("alias").or_insert_with(|| Value::String(alias));
    }
    if let Some(remark) = pick_string(map, &["remark", "Remark"]) {
        map.entry("remark").or_insert_with(|| Value::String(remark));
    }
    // nickName（camelCase）硬要求：格式化器只读此键
    if let Some(nick) = pick_string(map, CONTACT_NICK) {
        map.insert("nickName".to_string(), Value::String(nick));
    } else if !map.contains_key("nickName") {
        // 无昵称列：回退用 username 充当昵称，保证键存在
        if let Some(u) = &username {
            map.insert("nickName".to_string(), Value::String(u.clone()));
        }
    }
    // 分类判定三键（snake_case 首选）
    if let Some(v) = pick_string(map, &["local_type", "localType", "type", "ntype", "contact_type"]) {
        map.entry("local_type").or_insert_with(|| Value::String(v));
    }
    if let Some(v) = pick_string(map, &["flag", "contact_flag", "contactFlag"]) {
        map.entry("flag").or_insert_with(|| Value::String(v));
    }
    if let Some(v) = pick_string(map, &["quan_pin", "quanPin", "quanpin", "QuanPin"]) {
        map.entry("quan_pin").or_insert_with(|| Value::String(v));
    }
    // ① 实现方式：对行对象就地补齐协议推荐键——username/alias/remark/local_type/flag/
    //    quan_pin 用 entry().or_insert 只补缺；nickName 用 insert 强制写入，缺失时回退
    //    username 兜底。
    // ② 关键程序解析：推荐键走 or_insert（尊重库内原值），硬要求键走 insert（TS 格式化器
    //    只读 nickName camelCase，宁可回退 username 也要保证键存在）；值统一字符串化，
    //    分类三键供 TS 的 isOfficial/isSelf 等判定直接读。
    // ③ 功能：给 contact 行补齐协议要求的推荐键/硬要求键，跨 schema 口径对齐输出。
}

fn parse_usernames(json_str: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(json_str) {
        // 数组元素逐个转字符串：字符串 trim，非字符串（数字等）序列化后剥引号
        Ok(Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.trim().to_string()),
                other => Some(other.to_string().trim_matches('"').to_string()),
            })
            // 空项（trim 后为空）直接滤掉
            .filter(|s| !s.is_empty())
            .collect(),
        // 非数组/畸形 JSON 按空名单处理（调用方视为"无事可做"）
        _ => Vec::new(),
    }
    // ① 实现方式：serde_json 解析成 Value，仅接受数组形态；元素统一转字符串并过滤空项。
    // ② 关键程序解析：非字符串元素走 other.to_string() 再 trim_matches('"')——数字/布尔
    //    直接得文本，字符串套字符串（带引号形态）也能剥出内容；解析失败回空 Vec 而非报错，
    //    与协议"畸形入参给合法空结果"的降级约定一致。
    // ③ 功能：把 usernames_json / chatroom_ids_json 入参解析成 username 列表。
}

// ── wcdb_get_contact ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_contact(
    handle: i64,
    username: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let uname = cstr(username).trim().to_string();
        let result = (|| -> Result<String> {
            // 库定位失败直接报错（contact 库是必需库，缺失属异常而非降级场景）
            let Some(db) = contact_db_path(account) else {
                bail!("未找到 contact 库");
            };
            // 单名单查询取首行（单数主键理应至多一行）
            let rows = fetch_contacts(account, &db, std::slice::from_ref(&uname))?;
            match rows.into_iter().next() {
                // 命中：先补协议推荐键再序列化
                Some(Value::Object(mut m)) => {
                    add_contact_aliases(&mut m);
                    Ok(Value::Object(m).to_string())
                }
                Some(other) => Ok(other.to_string()),
                // 未找到：返回空对象（rc=0），消费方按空联系人处理，避免触发噪声 db dump
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：with_account 路由账号 → contact_db_path 定库 → fetch_contacts 按
        //    单元素名单查行 → 命中补 add_contact_aliases 后序列化，emit_json 统一出参。
        // ② 关键程序解析：未命中返回 "{}" 且 rc=0（协议约定"查无此人不是错误"，防 TS 端
        //    把 rc!=0 当库损坏触发噪声 dump）；库缺失才 bail 走 ERR_GENERIC；username 先
        //    trim 再查询，避免入参带空白导致 IN 匹配落空。
        // ③ 功能：FFI 入口——按 username 查单个联系人完整行并补齐协议推荐键。
    })
}

// ── wcdb_get_display_names ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_display_names(
    handle: i64,
    usernames_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let names = parse_usernames(cstr(usernames_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            // 空名单直接回空对象（合法 JSON，rc=0）
            if names.is_empty() {
                return Ok(Value::Object(out).to_string());
            }
            let Some(db) = contact_db_path(account) else {
                // 无 contact 库：回退 username 本身（TS 亦会回退）
                for n in &names {
                    out.insert(n.clone(), Value::String(n.clone()));
                }
                return Ok(Value::Object(out).to_string());
            };
            // 批量查行后逐行提取显示名：备注 > 昵称 > 别名，全缺则回退 username
            let rows = fetch_contacts(account, &db, &names)?;
            for row in rows {
                let Value::Object(m) = row else { continue };
                let Some(uname) = pick_string(&m, &["username", "user_name", "userName", "usrName"]) else {
                    continue;
                };
                let display = pick_string(&m, CONTACT_DISPLAY).unwrap_or_else(|| uname.clone());
                out.insert(uname, Value::String(display));
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：解析名单 → fetch_contacts 批量查行 → 逐行按 CONTACT_DISPLAY 候选链
        //    取显示名（缺失回退 username），组 {username: 显示名} 映射 emit_json 返回。
        // ② 关键程序解析：未命中的 username 不会出现在结果里，TS 端对缺键自行回退——
        //    与"无库时显式回填 username"共同保证显示名永不缺失；行内 username 列缺失/非
        //    对象行静默跳过，不影响其余行。
        // ③ 功能：FFI 入口——批量取多个 username 的会话显示名。
    })
}

// ── wcdb_get_avatar_urls ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_avatar_urls(
    handle: i64,
    usernames_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let names = parse_usernames(cstr(usernames_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            // 空名单/无库都回空对象（头像缺失由 TS 端用默认头像兜底，不算错误）
            if names.is_empty() {
                return Ok(Value::Object(out).to_string());
            }
            let Some(db) = contact_db_path(account) else {
                return Ok(Value::Object(out).to_string());
            };
            // 批量查行后逐行按 CONTACT_AVATAR 候选链（大图优先）取头像 URL
            let rows = fetch_contacts(account, &db, &names)?;
            for row in rows {
                let Value::Object(m) = row else { continue };
                let Some(uname) = pick_string(&m, &["username", "user_name", "userName", "usrName"]) else {
                    continue;
                };
                if let Some(url) = pick_string(&m, CONTACT_AVATAR) {
                    // 必须是 http(s) URL；拒绝 hex/base64 图片字节（协议 isValidAvatarUrl）
                    let u = url.trim();
                    if u.starts_with("http://") || u.starts_with("https://") {
                        out.insert(uname, Value::String(u.to_string()));
                    }
                }
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：解析名单 → 批量查行 → 逐行按大图 > 小图 > 通用头图候选链取 URL，
        //    过 http(s) 前缀校验后组 {username: url} 映射 emit_json 返回。
        // ② 关键程序解析：非 http(s) 开头的值（部分库把头像二进制存成 hex/base64）直接
        //    丢弃，杜绝 <img src> 挂上巨大 data 尾巴；校验口径与 TS 端 isValidAvatarUrl
        //    一致，双层防护；未命中/URL 非法都不写入，键缺失即"无自定义头像"。
        // ③ 功能：FFI 入口——批量取多个 username 的头像 URL。
    })
}

// ── 群成员解析（内省，双模型）──

// 模型 A：群"联系人"行内的成员列表/群昵称列表列候选（列结构未经 dbKey 校准，多口径并存）
const MEMBER_LIST_COLS: &[&str] = &[
    "user_name_list", "username_list", "chatroom_user_name_list", "chat_room_user_name_list",
    "usernamelist", "member_list", "user_list", "userNameList",
];
const MEMBER_DISPLAY_COLS: &[&str] = &[
    "display_name_list", "displayname_list", "chatroom_display_name_list",
    "chat_room_display_name_list", "displayNameList",
];
// 成员串分隔符候选：^ 是微信惯例，\n/;/|/, 及 \u{1}\u{2} 控制符是各版本历史口径
const SEPARATORS: &[char] = &['^', '\n', ';', '|', ',', '\u{1}', '\u{2}'];

fn split_members(s: &str) -> Vec<String> {
    // 多分隔符一次切分：trim 去首尾空白后滤空项
    s.split(SEPARATORS)
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
    // ① 实现方式：按 SEPARATORS 全部候选字符 split，逐项 trim 后过滤空串。
    // ② 关键程序解析：char 数组作 pattern 让一次 split 同时容忍所有历史分隔口径；
    //    尾随分隔符/连续分隔符产生的空项被 filter 滤掉，不影响成员计数。
    // ③ 功能：把群成员名串切分成 username 数组（模型 A 的解析核心）。
}

/// 群成员条目。移植改动：struct 与两个字段从私有提为 pub——
/// 本项目 wxmsg-exporter 的 --group-members 直接消费这两个字段。
pub struct MemberRow {
    pub username: String,
    pub display: Option<String>,
}
// ① 实现方式：username 必有；display 用 Option 表达"群昵称列缺失/未提供"两种态。
// ② 关键程序解析：模型 A 按下标对齐 username_list 与 display_name_list，display 可能越界
//    缺失取 None；模型 B 每成员一行，display 取自表内群昵称列（列缺失也 None）。
// ③ 功能：群成员条目（username + 可选群内昵称）的统一载体。

/// 解析群成员：
/// 模型 A —— 群"联系人"行内的 user_name_list / display_name_list（分隔符切分）；
/// 模型 B —— 每成员一行的表（含 chatroom_id + username 列）；
/// 模型 C —— 微信 4.1.15 实测的整数外键三表关联（chatroom_member.room_id → chat_room.id，
///           chatroom_member.member_id → name2id.rowid，成员 username 经 name2id 反查）。
/// 移植改动：fn 从私有提为 pub，供 wxmsg-exporter 调用；新增模型 C。
pub fn resolve_group_members(account: &Account, chatroom_id: &str) -> Result<Vec<MemberRow>> {
    // 无 contact 库：群成员无从解析，回空数组（调用方按 0 成员降级）
    let Some(db) = contact_db_path(account) else {
        return Ok(Vec::new());
    };
    // 模型 A
    if let Ok(rows) = fetch_contacts(account, &db, &[chatroom_id.to_string()]) {
        for row in rows {
            let Value::Object(m) = row else { continue };
            // 群行内命中成员列表列即按模型 A 解析
            if let Some(list) = pick_string(&m, MEMBER_LIST_COLS) {
                // username 串与群昵称串各自切分后按下标一一对齐
                let names = split_members(&list);
                let displays = pick_string(&m, MEMBER_DISPLAY_COLS)
                    .map(|d| split_members(&d))
                    .unwrap_or_default();
                let members = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, username)| MemberRow {
                        display: displays.get(i).cloned(),
                        username,
                    })
                    .collect();
                return Ok(members);
            }
        }
    }
    // 模型 B：在 contact 库中找带 chatroom_id 列的成员表
    let found = account.with_conn(&db, |conn| -> Result<Vec<MemberRow>> {
        let tables = list_tables(conn)?;
        // 逐表内省：同时具备群 id 列 + username 列才视为成员表
        for t in &tables {
            let cols = match table_columns(conn, t) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let room_col = find_candidate(
                &cols,
                &["chatroom_id", "chat_room_id", "room_id", "chatroomid", "chatRoomId"],
            );
            let user_col = find_candidate(&cols, &["username", "user_name", "userName", "usr_name"]);
            if let (Some(rc), Some(uc)) = (room_col, user_col) {
                // 表名/列名过白名单后拼接；群昵称列可选，存在才附加 SELECT 列
                let safe_t = sanitize_ident(t)?;
                let safe_rc = sanitize_ident(&rc)?;
                let safe_uc = sanitize_ident(&uc)?;
                let disp_col = find_candidate(&cols, &["display_name", "displayName", "nickname", "nick_name"]);
                let disp_sql = disp_col
                    .as_ref()
                    .map(|d| format!(", \"{}\"", sanitize_ident(d).unwrap_or_default()))
                    .unwrap_or_default();
                let sql = format!(
                    "SELECT \"{safe_uc}\"{disp_sql} FROM \"{safe_t}\" WHERE \"{safe_rc}\" = ?"
                );
                // 参数化查询该群全部成员行
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query([chatroom_id])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    // 空白 username 行跳过；群昵称列不存在时恒 None，存在但取失败也退化 None
                    let username: String = r.get(0)?;
                    if username.trim().is_empty() {
                        continue;
                    }
                    let display: Option<String> = if disp_col.is_some() { r.get(1).ok() } else { None };
                    out.push(MemberRow { username, display });
                }
                // 非空才算命中：空结果继续试下一张表（防止同名列的无关表误判）
                if !out.is_empty() {
                    return Ok(out);
                }
            }
        }
        Ok(Vec::new())
    })?;
    // 模型 C（微信 4.1.15 实测）：成员存成整数外键——chatroom_member(room_id, member_id)，
    // room_id → chat_room.id，member_id → name2id.rowid，成员 username 经 name2id 反查。
    // A/B 找的是文本形态（列/表），在 4.1.15 上全部落空，所以这里补整数关联这一路。
    // 三张表缺任一（旧版本微信）则跳过模型 C。
    if !found.is_empty() {
        return Ok(found);
    }
    let has_tables = account.with_conn(&db, |conn| {
        let tables = list_tables(conn)?;
        let names: Vec<String> = tables.iter().map(|t| t.to_ascii_lowercase()).collect();
        Ok(names.contains(&"chatroom_member".to_string())
            && names.contains(&"chat_room".to_string())
            && names.iter().any(|n| n == "name2id"))
    })?;
    if has_tables {
        let joined = account.with_conn(&db, |conn| -> Result<Vec<MemberRow>> {
            // 表名/列名均为诊断实测的固定值（4.1.15 schema），仍过 sanitize_ident 防拼接
            let (t_cm, t_cr, t_n2) = (
                sanitize_ident("chatroom_member")?,
                sanitize_ident("chat_room")?,
                sanitize_ident("name2id")?,
            );
            let (c_rm, c_mi, c_cr_id, c_cr_un, c_n2_un) = (
                sanitize_ident("room_id")?,
                sanitize_ident("member_id")?,
                sanitize_ident("id")?,
                sanitize_ident("username")?,
                // contact 库 name2id 的名称列是 username（message 库的 Name2Id 才是 user_name
                // ——同名异构，实测踩坑：列名写错会以 "no such column" 静默落进发言者池回退）
                sanitize_ident("username")?,
            );
            let sql = format!(
                "SELECT \"{t_n2}\".\"{c_n2_un}\" FROM \"{t_cm}\" \
                 JOIN \"{t_cr}\" ON \"{t_cm}\".\"{c_rm}\" = \"{t_cr}\".\"{c_cr_id}\" \
                 JOIN \"{t_n2}\" ON \"{t_n2}\".rowid = \"{t_cm}\".\"{c_mi}\" \
                 WHERE \"{t_cr}\".\"{c_cr_un}\" = ?"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query([chatroom_id])?;
            let mut out: Vec<MemberRow> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            while let Some(r) = rows.next()? {
                let username: String = r.get(0)?;
                if username.trim().is_empty() || !seen.insert(username.clone()) {
                    continue;
                }
                out.push(MemberRow { username, display: None });
            }
            Ok(out)
        })?;
        if !joined.is_empty() {
            return Ok(joined);
        }
    }
    Ok(found)
    // ① 实现方式：三模型级联——A 群联系人行成员列表列 → B 文本成员表（chatroom_id+username
    //    列）→ C 整数外键关联（4.1.15 实测：chatroom_member.room_id→chat_room.id，
    //    chatroom_member.member_id→name2id.rowid，username 经 name2id 反查）。
    // ② 关键程序解析：模型 A 的 display 与 username 按切分后下标一一对应，越界取 None；
    //    模型 B 命中后仍要求查询结果非空才返回；模型 C 用 INNER JOIN（name2id 查不到的
    //    成员 ID 被丢弃）+ seen 去重；三模型全部失败回空数组；全部 SQL 标识符过
    //    sanitize_ident，群 id 走 ? 绑定。
    // ③ 功能：解析群成员名单（username + 可选群昵称），成员数/成员列表导出的公共数据层。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_member_count(
    handle: i64,
    chatroom_id: *const c_char,
    out_count: *mut i32,
) -> i32 {
    with_account(handle, |account| {
        // 出参防御：NULL 槽直接拒；先写 0 保证任何路径下出参都有合法值
        if out_count.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_count = 0 };
        let room = cstr(chatroom_id).trim().to_string();
        match resolve_group_members(account, &room) {
            Ok(members) => {
                // usize 长度收敛进 i32 正域（usize > i32::MAX 在 32 位出参上会溢出回绕）
                let n = members.len().clamp(0, i32::MAX as usize) as i32;
                unsafe { *out_count = n };
                OK
            }
            Err(e) => {
                push_log(&format!("get_group_member_count: {e}"));
                // 未知 schema 返回 0 而非错误，避免上层降级噪声
                unsafe { *out_count = 0 };
                OK
            }
        }
        // ① 实现方式：with_account 路由账号 → resolve_group_members 双模型解析 → 成员数
        //    clamp 后写 i32 出参；解析失败只记日志、出参保持 0 且仍返 OK。
        // ② 关键程序解析：唯一 unsafe 的两次写都限 i32 出参槽，NULL 已前置拒绝；失败返回
        //    OK(0 个成员) 而非错误码——群成员表 schema 未校准，报错会让 TS 认为账号异常
        //    触发噪声降级，属协议约定的"合法空结果"；详情进环形日志可查。
        // ③ 功能：FFI 入口——取单个群的成员数量。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_member_counts(
    handle: i64,
    chatroom_ids_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let ids = parse_usernames(cstr(chatroom_ids_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            // 逐群解析成员数；单群失败 unwrap_or(0) 记 0，不拖垮整批
            for id in &ids {
                let n = resolve_group_members(account, id).map(|m| m.len()).unwrap_or(0);
                out.insert(id.clone(), Value::Number((n as i64).into()));
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：解析群 id 名单后逐个调 resolve_group_members 取 len()，组
        //    {chatroom_id: 成员数} 映射 emit_json 返回（可选符号，TS 侧可能不调）。
        // ② 关键程序解析：与单群 count 不同，失败以 0 内联进结果而非整体 rc 失败，
        //    保证一次调用就能铺满会话列表角标；n 以 i64 表达避免 32 位截断。
        // ③ 功能：FFI 入口——批量取多个群的成员数量（fail-soft）。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_members(
    handle: i64,
    chatroom_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let room = cstr(chatroom_id).trim().to_string();
        let result = (|| -> Result<String> {
            // 解析失败 unwrap_or_default 回空数组——成员名单缺失不作为错误上抛
            let members = resolve_group_members(account, &room).unwrap_or_default();
            let arr: Vec<Value> = members
                .into_iter()
                // 每成员组一个对象；username 恒写入，群昵称非空才双键写入
                .map(|m| {
                    let mut o = Map::new();
                    o.insert("username".to_string(), Value::String(m.username.clone()));
                    if let Some(d) = m.display {
                        if !d.trim().is_empty() {
                            o.insert("nickname".to_string(), Value::String(d.clone()));
                            o.insert("displayName".to_string(), Value::String(d));
                        }
                    }
                    Value::Object(o)
                })
                .collect();
            Ok(Value::Array(arr).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：resolve_group_members 双模型解析后逐成员组 {username, nickname?,
        //    displayName?} 对象数组，emit_json 返回；任何失败都产出合法 "[]"。
        // ② 关键程序解析：nickname（snake）与 displayName（camel）双键同值，兼容 TS 侧
        //    两种读法；群昵称空白视为无昵称不写键，消费方按键存在与否回退 username。
        // ③ 功能：FFI 入口——取单个群的完整成员名单。
    })
}

// ── wcdb_get_emoticon_cdn_url（纯字符串输出）──

/// 解析表情库路径：dbPath 可能是 emoticon.db 全路径、账号根目录、db_storage 上层目录。
fn resolve_emoticon_db(account: &Account, db_hint: &str) -> Option<PathBuf> {
    let hint = db_hint.trim();
    if !hint.is_empty() {
        let p = PathBuf::from(hint);
        // 提示本身已是 .db 文件：直接采信
        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("db") {
            return Some(p);
        }
        // 目录提示：向下找 emoticon.db / emotion/emoticon.db
        for sub in [
            PathBuf::from("emoticon").join("emoticon.db"),
            PathBuf::from("emotion").join("emoticon.db"),
            PathBuf::from("db_storage").join("emoticon").join("emoticon.db"),
            PathBuf::from("emoticon.db"),
        ] {
            let cand = p.join(&sub);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 提示不可用：退账号布局登记路径，仍要求磁盘存在
    if let Some(p) = &account.layout.emoticon_db {
        if p.is_file() {
            return Some(p.clone());
        }
    }
    // 最后回退：账号目录下 db_storage/{emoticon,emotion}/emoticon.db 两种历史目录名
    let dir = account.account_dir()?;
    for sub in ["emoticon", "emotion"] {
        let cand = dir.join("db_storage").join(sub).join("emoticon.db");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
    // ① 实现方式：三级回退定位表情库——dbPath 提示（.db 文件直用 / 目录下四个候选子路径
    //    试探）→ account.layout.emoticon_db → 账号目录 db_storage/{emoticon,emotion} 约定路径。
    // ② 关键程序解析：dbPath 可能是 emoticon.db 全路径、账号根目录或 db_storage 上层目录
    //    三种形态，故文件/目录两分支都要兜；每一级都 is_file 校验，绝不带不存在的路径去
    //    开连接；全落空返回 None，上层写空串。
    // ③ 功能：解析表情库真实磁盘路径（表情 CDN 导出的数据源定位）。
}

fn lookup_emoticon_url(account: &Account, db: &Path, md5: &str) -> Option<String> {
    account
        .with_conn(db, |conn| -> Result<Option<String>> {
            // 表/列三级内省：表情表、md5 列、CDN URL 列任一缺失都按"查不到"返回（非报错）
            let Some(table) = locate_table(conn, &["emoticon", "emotion", "Emoticon", "emoji"])
            else {
                return Ok(None);
            };
            let cols = table_columns(conn, &table)?;
            let Some(md5_col) = find_candidate(&cols, &["md5", "md5_key", "str_md5", "key", "MD5"])
            else {
                return Ok(None);
            };
            let Some(url_col) = find_candidate(
                &cols,
                &["cdn_url", "cdnurl", "cdn_url_2", "cdn_url_1", "url", "cdnUrl", "CdnUrl"],
            ) else {
                return Ok(None);
            };
            // 标识符白名单校验：任一列名含非法字符即放弃（Ok(..) 模式失败走 else 分支）
            let (Ok(u), Ok(t), Ok(m)) = (
                sanitize_ident(&url_col),
                sanitize_ident(&table),
                sanitize_ident(&md5_col),
            ) else {
                return Ok(None);
            };
            // SQL 拼接仅含白名单标识符，md5 走 ? 绑定；LIMIT 1 只要一条
            let sql = format!("SELECT \"{u}\" FROM \"{t}\" WHERE \"{m}\" = ? LIMIT 1");
            let mut stmt = conn.prepare(&sql)?;
            // md5 大小写不敏感匹配
            for cand in [md5.to_string(), md5.to_ascii_lowercase(), md5.to_ascii_uppercase()] {
                if let Ok(url) = stmt.query_row([&cand], |r| r.get::<_, String>(0)) {
                    if !url.trim().is_empty() {
                        return Ok(Some(url));
                    }
                }
            }
            Ok(None)
        })
        .unwrap_or(None)
    // ① 实现方式：locate_table 定表情表 → find_candidate 定 md5/CDN URL 列 → 拼
    //    SELECT cdn_url WHERE md5=? LIMIT 1 → 按 原样/小写/大写 三形态绑定查询。
    // ② 关键程序解析：内省失败与 sanitize 失败都收敛为 Ok(None)（表情表 schema 未经
    //    dbKey 校准，"查不到"是常态而非异常）；三形态循环覆盖库内 md5 大小写不定的历史
    //    口径；连接级错误 unwrap_or(None) 同样归为 miss。
    // ③ 功能：按 md5 在表情库中查 CDN 下载地址。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_emoticon_cdn_url(
    handle: i64,
    db_path: *const c_char,
    md5: *const c_char,
    out_url: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let hint = cstr(db_path);
        let md5 = cstr(md5).trim().to_string();
        // 纯字符串输出：未命中写空串（rc=0，TS 视为 url=undefined）
        let url = match resolve_emoticon_db(account, hint) {
            Some(db) => lookup_emoticon_url(account, &db, &md5).unwrap_or_default(),
            None => String::new(),
        };
        write_out(out_url, &url);
        OK
        // ① 实现方式：with_account 路由账号 → resolve_emoticon_db 三级回退定库 →
        //    lookup_emoticon_url 按 md5 查 CDN 地址 → write_out 写出参，恒返 OK。
        // ② 关键程序解析：本函数永不失败——库定位失败与查询 miss 都写空串返回 OK，TS
        //    侧空串即视为无 CDN 地址、走表情包兜底下载；这保证 initialize 不因表情库
        //    缺失/ schema 差异中断；出参指针由 alloc_out 分配，TS 经 wcdb_free_string 归还。
        // ③ 功能：FFI 入口——按 md5 取表情包的 CDN 下载 URL（纯字符串出参）。
    })
}
