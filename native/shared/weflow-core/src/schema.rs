//! 微信 4.x 消息库 schema 发现与会话→消息表路由。
//!
//! 真实表结构需拿到 dbKey 后用 <微信数据根> 验证（任务 #7），因此这里采取
//! "运行时内省 + 多候选列名"策略：
//! - 消息分表：名字形如 `MSG_*`/`Msg_*`/`message_*`（排除 FTS 辅助表）；
//! - 会话路由两种模式：
//!   A. 映射表模式：存在 Name2ID 类表（username → int id），消息表按 id 命名（MSG_{id}）；
//!   B. 共享表模式：消息表内有 talker 列（str_talker/talker/session_id/username 等），
//!      用 WHERE 过滤。
//! 两种都不满足时返回 schema mismatch（状态码 -7 由调用方映射）。

use crate::db::{list_tables, Account};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NAME_COL_CANDIDATES: &[&str] = &[
    "usr_name",
    "usrname",
    "user_name",
    "username",
    "str_session_id",
    "strsessionid",
    "session_id_text",
];
// ① 实现方式：&[&str] 常量数组，**顺序即优先级**，由 find_candidate 逐个做大小写不敏感匹配。
// ② 关键程序解析：覆盖微信各版本对"会话 username 列"的命名差异（usr_name / user_name / str_session_id 等）；
//    排在前面的先命中，所以微信 4.x 的 usr_name 优先于泛化的 username。
//    这是"运行时内省 + 多候选"策略的核心数据——不做硬编码单一列名，微信改列名时只需往数组里加一项。
// ③ 功能：在 Name2ID 类映射表里找出"会话名"列。
const ID_COL_CANDIDATES: &[&str] = &["rowid", "id", "usr_id", "usrid", "session_id", "sessionid", "talker_id", "talkerid"];
// ① 实现方式：同样是有序候选数组，用于映射表的"整数 id"列。
// ② 关键程序解析：`rowid` 排第一，但 PRAGMA table_info **不会列出隐式 rowid**，
//    所以只有显式声明了 rowid 列的表才会命中它；否则 find_candidate 返回 None，
//    MappingTable.id_col=None，查询时再退回字面量 "rowid"（SQLite 隐式列，可直接 SELECT）。
//    坑：若表里有个与消息表后缀无关的 `id`（如自增主键），会被当成消息表编号用，路由到错误的表。
// ③ 功能：找出映射表里可作为消息表名后缀的整数 id 列。
const TALKER_COL_CANDIDATES: &[&str] = &[
    "str_talker",
    "strtalker",
    "talker",
    "session_id",
    "sessionid",
    "str_session_id",
    "username",
    "user_name",
];
// ① 实现方式：共享表模式（模式 B）下用于 WHERE 过滤的 talker 列候选，顺序即优先级。
// ② 关键程序解析：只在 md5 主路由与映射表路由都落空时才用到；MsgIndex::build 只抽样
//    **第一张**消息分表来探这一列，并假定所有分表结构一致——若某张分表列名不同，
//    共享表模式下对它拼出的 WHERE 会直接报"no such column"。
// ③ 功能：为"所有会话共用一张消息表"的旧布局提供过滤列。
/// annualReportService.detectTimeColumn 的候选序（04 规格 §message_table_columns）。
const TIME_COL_CANDIDATES: &[&str] = &[
    "create_time",
    "createtime",
    "msg_create_time",
    "msg_time",
    "msgtime",
    "time",
];
// ① 实现方式：时间列候选数组，与 TS 侧 annualReportService.detectTimeColumn 的候选序保持一致。
// ② 关键程序解析：注意这里**没有 sort_seq**——messages.rs 的 C_SORT_SEQ/C_CREATE_TIME 是另一套更宽的候选，
//    两处口径不同：本数组只服务 detect_time_column（年度报告/统计类时间轴），
//    消息流排序用的是 messages.rs 那套（优先 sort_seq）。改一处不会自动同步另一处。
// ③ 功能：在消息/朋友圈表里定位创建时间列。

fn is_msg_table(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("sqlite_") {
        return false;
    }
    // ① 实现方式：先转小写再判前缀，`sqlite_` 开头一律排除。
    // ② 关键程序解析：sqlite_* 是 SQLite 内部表（sqlite_sequence/sqlite_stat1 等），
    //    虽然 list_tables 已经只取 type='table'，但内部表同样是 'table'，必须在这里挡掉；
    //    转小写让 `MSG_`/`Msg_`/`msg_` 三种写法统一处理。
    // ③ 功能：排除引擎内部表。
    // 排除 FTS 辅助表（*_content/*_segments/*_idx/*_docsize/*_config/*_data 等）
    let is_fts_part = ["_content", "_segments", "_segdir", "_idx", "_docsize", "_config", "_data", "_stat"]
        .iter()
        .any(|suffix| lower.ends_with(suffix));
    if is_fts_part {
        return false;
    }
    // ① 实现方式：用后缀黑名单数组 + any() 判定是否为 FTS 影子表，命中即排除。
    // ② 关键程序解析：FTS5 虚表会自动派生 <name>_content/_data/_idx/_docsize/_config 等真实表，
    //    它们都以 message/Msg 开头时会被前缀规则误收，故必须先按后缀剔除；
    //    **但虚表本体（如 `message_fts`）不在黑名单里**——它在 sqlite_master 中也是 type='table'，
    //    若它恰好落在 message_N.db 内就会被当成消息分表（待实测：本机 FTS 独立成 message_fts.db，故未触发）。
    // ③ 功能：防止把 FTS 辅助表当消息表去 SELECT/排序。
    lower.starts_with("msg") || lower.starts_with("message")
    // ① 实现方式：最后判前缀，`msg*` 或 `message*` 即认为是消息分表。
    // ② 关键程序解析：`message` 并不以 `msg` 开头（m-e-s vs m-s-g），所以两个前缀条件都必须写；
    //    宽松前缀意味着 `MsgHashIndex` 这类非消息表也可能被收进来，
    //    真正的会话定位靠后续 md5 后缀匹配再筛一遍。
    // ③ 功能：给出"是不是消息分表"的布尔判据，供 MsgIndex 建索引与路由复用。
}

/// 会话 username 对应的消息表名后缀：md5(username) 的小写十六进制（微信 4.x 实测规律）。
/// 真实表名形如 `Msg_<md5hex>`，落在包含它的那个 message_N.db 分库中。
pub fn msg_table_hash(session_id: &str) -> String {
    format!("{:x}", md5::compute(session_id.as_bytes()))
    // ① 实现方式：对 session_id 的原始 UTF-8 字节算 MD5，用 `{:x}` 格式化成 32 位**小写**十六进制。
    // ② 关键程序解析：不做 trim、不做大小写归一——微信是按 username 原样算的哈希，
    //    调用方传进来的 sid 若带首尾空格或大小写不同，哈希就完全不同，路由会静默退化成 "none"（0 条消息）。
    //    群聊 id 含 `@chatroom` 后缀也一起参与哈希。MD5 在这里只是命名规则，不承担任何安全职责。
    // ③ 功能：算出消息分表名 `Msg_<md5hex>` 的后缀，是微信 4.x 会话→表的主路由键。
}

#[derive(Debug, Clone)]
pub struct MappingTable {
    pub db_idx: usize,
    pub table: String,
    pub name_col: String,
    /// None = 用 SQLite 隐式 rowid。
    pub id_col: Option<String>,
    // ① 实现方式：记录一张 Name2ID 类映射表的位置与两个关键列名；db_idx 指向 MsgIndex.dbs 的下标。
    // ② 关键程序解析：name_col 必有（resolve_mapping 找不到就返回 None），id_col 可为 None
    //    表示用隐式 rowid；三个字符串都来自 PRAGMA 内省结果（原始大小写），
    //    拼进 SQL 前必须过 sanitize_ident。整个结构只描述"表在哪、列叫什么"，不缓存任何行数据。
    // ③ 功能：支撑模式 A 的兜底路由——先由 username 查整数 id，再拼 `MSG_{id}` 找消息表。
}

#[derive(Debug, Clone)]
pub struct MsgIndex {
    /// 与 Account.layout.message_dbs 对齐。
    pub dbs: Vec<PathBuf>,
    pub tables_per_db: Vec<Vec<String>>,
    // ① 实现方式：dbs 是消息分库路径快照，tables_per_db 是**等长平行数组**（下标即 db_idx）。
    // ② 关键程序解析：两者必须严格对齐，db_path(db_idx) 直接按下标取 dbs；
    //    tables_per_db 存的是该库**全部**表名（含非消息表），路由时再用 is_msg_table 过滤，
    //    这样 md5 后缀匹配能看到原始大小写的真实表名。索引是构建时快照，微信新建分表后不会自动更新。
    // ③ 功能：提供"分库 → 表清单"的静态视图。
    /// 全部消息分表 (db_idx, table)。
    pub msg_tables: Vec<(usize, String)>,
    pub mapping: Option<MappingTable>,
    /// 共享表模式下的 talker 过滤列。
    pub talker_column: Option<String>,
    // ① 实现方式：msg_tables 是预筛过的消息分表扁平列表；mapping/talker_column 是两种兜底路由的依据。
    // ② 关键程序解析：mapping 只保留**第一个**成功解析的 Name2ID 表（build 里 `mapping.is_none()` 守卫）；
    //    talker_column 只抽样第一张消息分表得到，假定全部分表同构。
    //    三者共同决定 session_tables_routed 走 md5 / mapping / shared / none 哪一级。
    // ③ 功能：把路由所需的全部 schema 事实一次性固化，避免每次读消息都重新内省。
}

/// 会话的消息表定位结果。
#[derive(Debug, Clone)]
pub enum SessionTables {
    /// 模式 A：专属分表已定位。
    Located(Vec<(usize, String)>),
    /// 模式 B：所有消息分表 + talker 过滤列。
    Shared { tables: Vec<(usize, String)>, talker_column: String },
    // ① 实现方式：二选一 enum——Located 携带精确的 (db_idx, 表名) 列表；Shared 携带全部消息表 + 过滤列名。
    // ② 关键程序解析：Located 可以是**空 Vec**（合法语义："该会话本机没有消息表"，不是错误）；
    //    Shared 必须配 talker_column，messages.rs 据此给每张表加 `WHERE "<col>" = ?`（值走参数绑定）。
    //    两种模式在消费侧的处理完全不同：Located 不加过滤，Shared 必须加，否则会读到别人的消息。
    // ③ 功能：把"到哪几张表读、要不要按 talker 过滤"这件事表达成类型安全的出参。
}

impl MsgIndex {
    pub fn build(account: &Account) -> Result<Arc<MsgIndex>> {
        let dbs = account.layout.message_dbs.clone();
        if dbs.is_empty() {
            bail!("未发现 message_*.db 分库");
        }
        // ① 实现方式：克隆布局里的消息分库清单作为索引的 dbs 快照；空清单直接 bail。
        // ② 关键程序解析：克隆而非借用，让 MsgIndex 脱离 Account 的生命周期独立存在（Arc 缓存）；
        //    空清单的成因可能是 discover_layout 没扫到（目录改名）或账号确实没有消息库，
        //    这里统一报"未发现分库"，具体哪项缺失要看 DbLayout::summary。
        // ③ 功能：确定要内省的分库范围并守住下界。
        let mut tables_per_db = Vec::with_capacity(dbs.len());
        let mut msg_tables = Vec::new();
        let mut mapping: Option<MappingTable> = None;
        // ① 实现方式：预分配与 dbs 等长的平行数组，另建消息分表扁平列表与映射表槽位。
        // ② 关键程序解析：tables_per_db 必须与 dbs 下标一一对应（后面 db_path 按下标取），
        //    所以每轮循环**无论是否找到消息表都要 push 一项**；mapping 初始 None，只认第一个命中。
        // ③ 功能：准备三个累积容器。

        for (db_idx, db) in dbs.iter().enumerate() {
            let tables = account.with_conn(db, |conn| list_tables(conn))?;
            // ① 实现方式：按序号升序遍历分库，每库通过 with_conn 取连接后 list_tables 拿全部表名。
            // ② 关键程序解析：`?` 让**任一分库打不开就整体失败**（例如某库密钥缺失且主 key 打不开），
            //    不是跳过该库继续——这会导致索引构建全有或全无；with_conn 内部已按每库密钥表选钥。
            // ③ 功能：逐库内省表清单。
            for t in &tables {
                let lower = t.to_ascii_lowercase();
                if mapping.is_none() && lower.contains("name2id") {
                    if let Ok(cols) = account.columns(db, t) {
                        if let Some(m) = resolve_mapping(t, &cols, db_idx) {
                            mapping = Some(m);
                        }
                    }
                }
                // ① 实现方式：表名含 "name2id"（大小写不敏感）且尚未找到映射表时，取列名并尝试 resolve_mapping。
                // ② 关键程序解析：三层嵌套全部是"失败就静默跳过"——columns 出错、找不到 name 列，
                //    都只是不设置 mapping，不影响主流程；`mapping.is_none()` 守卫保证只认第一个，
                //    因此多个 Name2ID 表并存时结果取决于 list_tables 的 name 升序。
                // ③ 功能：发现模式 A 兜底路由所需的 username→id 映射表。
                if is_msg_table(t) {
                    msg_tables.push((db_idx, t.clone()));
                }
                // ① 实现方式：用 is_msg_table 过滤后把 (db_idx, 原始表名) 收进扁平列表。
                // ② 关键程序解析：保留原始大小写（后续要拼进 SQL），排除 sqlite_* 与 FTS 影子表；
                //    同一张表不会重复收录（每库表名唯一）。
                // ③ 功能：积累全部消息分表，供共享表模式与抽样探列使用。
            }
            tables_per_db.push(tables);
        }
        // ① 实现方式：每库遍历结束后把完整表清单 push 进平行数组。
        // ② 关键程序解析：push 的是**未过滤**的全量表名，md5 路由要在其中按后缀匹配；
        //    顺序 = dbs 顺序 = 编号升序，这决定了跨分库读消息时的先后。
        // ③ 功能：固化"分库 → 表清单"视图。

        // talker 列探测：抽样第一张消息分表
        let mut talker_column = None;
        if let Some((db_idx, table)) = msg_tables.first() {
            if let Ok(cols) = account.columns(&dbs[*db_idx], table) {
                talker_column = find_candidate(&cols, TALKER_COL_CANDIDATES);
            }
        }
        // ① 实现方式：只取 msg_tables 的第一张表，查其列名并按候选序找 talker 列。
        // ② 关键程序解析：抽样一张即假定所有分表同构（微信 4.x 成立）；columns 失败或候选全落空
        //    都保持 None，此时共享表模式不可用。第一张表由 list_tables 的 name 升序决定，
        //    ASCII 序下大写 `Msg_*` 会排在小写表之前，所以抽样对象是稳定的。
        // ③ 功能：为模式 B 预备过滤列。

        Ok(Arc::new(MsgIndex { dbs, tables_per_db, msg_tables, mapping, talker_column }))
        // ① 实现方式：把五项累积结果打包成 MsgIndex 并裹进 Arc 返回。
        // ② 关键程序解析：msg_tables 为空**不算错误**（后面 session_tables_routed 才判"未发现消息分表"），
        //    因此索引可能构建成功但完全不可路由；Account::msg_index 会缓存这个快照，
        //    微信之后新建的分表必须靠 invalidate_msg_index 才能进入视野。
        // ③ 功能：交付可复用的分表索引。
    }

    /// 定位 sessionId 的消息分表。定位不到（映射无此会话）返回空向量而非错误。
    pub fn session_tables(&self, account: &Account, session_id: &str) -> Result<SessionTables> {
        self.session_tables_routed(account, session_id).map(|(t, _)| t)
        // ① 实现方式：转调 routed 版本，用 map 丢掉路由名只留定位结果。
        // ② 关键程序解析：保留这个不带路由的入口是为了兼容旧调用方；诊断/导出链路应改用
        //    session_tables_routed，才能看出"从哪一级路由退化的"。
        // ③ 功能：给出会话的消息表定位（模式 A 精确列表或模式 B 共享表 + 过滤列）。
    }

    /// 同 `session_tables`，但把**命中的是哪一级路由**带出来：
    /// `md5`（主路由）/ `mapping`（映射表）/ `shared`（共享表 + talker 列）/ `none`（合法空）。
    /// 微信下次改布局时，这条就是"从哪一级退化的"的第一现场证据。
    pub fn session_tables_routed(
        &self,
        account: &Account,
        session_id: &str,
    ) -> Result<(SessionTables, &'static str)> {
        // 主路由（微信 4.x 实测）：消息表名 = `Msg_<md5(session_id)>`，按哈希后缀在各分库中定位。
        let hash = msg_table_hash(session_id);
        let mut located: Vec<(usize, String)> = Vec::new();
        for (db_idx, tables) in self.tables_per_db.iter().enumerate() {
            for t in tables {
                if is_msg_table(t) && t.to_ascii_lowercase().ends_with(&hash) {
                    located.push((db_idx, t.clone()));
                }
            }
        }
        // ① 实现方式：算出 md5 后缀，双层遍历所有分库的表清单，用「is_msg_table + 小写后 ends_with(hash)」双条件筛选。
        // ② 关键程序解析：ends_with 而非全等，是为了兼容 `Msg_<hash>` 之外的前缀变体；
        //    比较前把表名转小写（hash 本身已是小写），所以 `MSG_`/`Msg_` 都能命中。
        //    遍历的是**构建时快照** tables_per_db，微信新建的会话表不在其中 → 这里空手而归。
        // ③ 功能：执行微信 4.x 的主路由（会话专属分表定位）。
        if !located.is_empty() {
            located.sort();
            located.dedup();
            return Ok((SessionTables::Located(located), "md5"));
        }
        // ① 实现方式：命中则先 sort 再 dedup（dedup 要求有序才能去掉全部重复），返回 Located + 路由名 "md5"。
        // ② 关键程序解析：sort 按 (db_idx, 表名) 升序，保证读消息时分库顺序稳定；
        //    同一会话理论上只有一张表，多命中通常是同名表出现在多个分库（历史迁移），此时会**都读**再合并。
        // ③ 功能：主路由命中即返回，不再尝试任何兜底。

        if let Some(mapping) = &self.mapping {
            let db = &self.dbs[mapping.db_idx];
            let id_col = mapping.id_col.clone().unwrap_or_else(|| "rowid".to_string());
            // ① 实现方式：主路由落空后进入模式 A 兜底——取映射表所在分库，id 列缺省回退字面量 "rowid"。
            // ② 关键程序解析：`self.dbs[mapping.db_idx]` 是直接下标索引，依赖 build 时两者严格对齐（不会越界）；
            //    id_col=None 只发生在映射表没有显式 id 列的情况，此时用 SQLite 隐式 rowid。
            // ③ 功能：准备"由 username 查整数 id"的查询目标。
            let sql = format!(
                "SELECT \"{}\" FROM \"{}\" WHERE \"{}\" = ? LIMIT 1",
                sanitize_ident(&id_col)?,
                sanitize_ident(&mapping.table)?,
                sanitize_ident(&mapping.name_col)?
            );
            // ① 实现方式：拼 `SELECT "<id_col>" FROM "<table>" WHERE "<name_col>" = ? LIMIT 1`，
            //    三个标识符都先过 sanitize_ident 白名单，只有 session_id 走 ? 参数绑定。
            // ② 关键程序解析：标识符不能用 ? 绑定，所以白名单是这里唯一的注入防线；
            //    三个 `?`（sanitize_ident 的 Result）任一失败即整体 bail，不会拼出半条 SQL。
            //    LIMIT 1 因为 username 在映射表里唯一；取不到就是 None（该会话不在映射表里）。
            // ③ 功能：构造安全的映射查询 SQL。
            let found: Option<i64> = account.with_conn(db, |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let found = stmt
                    .query_map([session_id], |r| r.get::<_, i64>(0))?
                    .next()
                    .transpose()?;
                Ok(found)
            })?;
            // ① 实现方式：在映射表所在库上 prepare + query_map，绑定 session_id，取第一行第 0 列成 i64。
            // ② 关键程序解析：`.next().transpose()?` 把 Option<Result<i64>> 翻成 Result<Option<i64>>，
            //    从而"没有行"是 Ok(None) 而"行解码失败"是 Err；整段再被 with_conn 的 `?` 冒泡。
            //    列类型若不是整数（如 TEXT id），r.get::<i64> 会失败并让整条路由报错，而不是静默跳过。
            // ③ 功能：查出该会话对应的整数 id。
            if let Some(id) = found {
                let mut located = Vec::new();
                for prefix in ["MSG_", "Msg_", "msg_", "MESSAGE_", "message_"] {
                    let name = format!("{prefix}{id}");
                    // ① 实现方式：查到 id 后，依次用五种大小写/拼写前缀拼出候选表名 `前缀+id`。
                    // ② 关键程序解析：前缀数组覆盖历史版本的命名差异；注意这里是**整数 id 拼接**，
                    //    与主路由的 md5 后缀完全不同，两套命名规则不能混用。
                    // ③ 功能：生成模式 A 的候选消息表名。
                    for (db_idx, tables) in self.tables_per_db.iter().enumerate() {
                        if tables.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                            let real = tables
                                .iter()
                                .find(|t| t.eq_ignore_ascii_case(&name))
                                .cloned()
                                .unwrap_or(name.clone());
                            located.push((db_idx, real));
                        }
                    }
                    // ① 实现方式：在每个分库的表清单里做大小写不敏感查找，命中后取**数据库里的真实表名**收录。
                    // ② 关键程序解析：先 any() 再 find() 是两次线性扫描（表数量级为百，代价可接受）；
                    //    unwrap_or(name.clone()) 是理论不可达的兜底（any 已保证 find 必中）。
                    //    取真实名而非拼接名，是为了后续 SQL 拼出的标识符与磁盘 schema 完全一致。
                    // ③ 功能：把候选表名落实成 (db_idx, 真实表名)。
                }
                located.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
                // ① 实现方式：用 dedup_by 去掉相邻的重复 (db_idx, 表名)。
                // ② 关键程序解析：五个前缀会命中同一张真实表，因此必须去重；但 dedup_by **只比较相邻元素**
                //    （不像 sort+dedup），当同一张表名出现在多个分库时，重复项可能不相邻而漏删 →
                //    同一张表被读多遍、消息重复。单分库场景下重复项相邻，去重正常。
                // ③ 功能：避免同表重复读取。
                if !located.is_empty() {
                    return Ok((SessionTables::Located(located), "mapping"));
                }
                // ① 实现方式：去重后非空即返回 Located + 路由名 "mapping"。
                // ② 关键程序解析：这里没有 sort，顺序 = 前缀数组顺序 × 分库顺序；
                //    映射表给出的 id 未经任何"这张表确实属于该会话"的二次校验，
                //    若 id 列语义不符（如自增主键），会静默路由到**别的会话**的表。
                // ③ 功能：模式 A 兜底命中出口。
            }
            // 映射表存在但没查到该会话：可能仍是共享表模式，继续向下尝试
        }
        // ① 实现方式：整个 mapping 分支是"尽力而为"——查不到 id 或拼不出表名都不报错，继续往下走。
        // ② 关键程序解析：这是路由降级链的第二级；只有 with_conn/prepare 这类硬错误才会冒泡中断。
        // ③ 功能：保证映射表失效时仍能尝试共享表模式。
        if let Some(col) = &self.talker_column {
            return Ok((
                SessionTables::Shared { tables: self.msg_tables.clone(), talker_column: col.clone() },
                "shared",
            ));
        }
        // ① 实现方式：有 talker 列就返回 Shared——把**全部**消息分表交给消费侧，并带上过滤列名。
        // ② 关键程序解析：克隆整个 msg_tables（可能上千项），消费侧必须给每张表加 WHERE 过滤，
        //    否则会读到所有会话的消息；这一级是最后的功能性兜底，命中它说明布局已偏离微信 4.x 主规则。
        // ③ 功能：模式 B（共享表）路由出口。
        if self.msg_tables.is_empty() {
            bail!("消息库中未发现消息分表");
        }
        // ① 实现方式：三级路由全落空且连一张消息分表都没有 → bail（硬错误）。
        // ② 关键程序解析：与下面"合法空"区分开——有表但没这个会话是正常，一张表都没有说明 schema 完全不符，
        //    调用方据此映射 schema mismatch 状态码。
        // ③ 功能：把"结构完全不对"与"该会话没消息"两种情况分开报。
        // md5 主路由与旧兜底都未命中：该会话在本机没有对应消息表（如历史已清空、
        // 服务号/资讯号等），属合法的"0 条消息"，返回空定位而非错误。
        Ok((SessionTables::Located(Vec::new()), "none"))
        // ① 实现方式：返回空 Located + 路由名 "none"。
        // ② 关键程序解析：空 Vec 会让消费侧一张表都不读、返回 0 条——这是**静默**结果，
        //    所以路由名必须一路带到导出 JSON 里，否则"新会话表没进索引"与"真的没消息"无法区分。
        // ③ 功能：合法空定位的统一出口。
    }

    pub fn db_path(&self, db_idx: usize) -> Option<&Path> {
        self.dbs.get(db_idx).map(PathBuf::as_path)
        // ① 实现方式：用 get 做安全下标访问，再把 &PathBuf 转成 &Path 返回。
        // ② 关键程序解析：越界返回 None 而非 panic；返回借用的生命周期绑定 &self，
        //    调用方（messages.rs）拿到后立刻 to_path_buf 克隆，避免长期借用索引。
        // ③ 功能：把路由结果里的 db_idx 还原成实际库文件路径。
    }
}

fn resolve_mapping(table: &str, cols: &[String], db_idx: usize) -> Option<MappingTable> {
    let name_col = find_candidate(cols, NAME_COL_CANDIDATES)?;
    // ① 实现方式：先按候选序找"会话名列"，找不到用 `?` 直接返回 None（这张表不是可用映射表）。
    // ② 关键程序解析：name 列是**必需**条件——没有它就无法用 username 查 id；
    //    返回的列名保留数据库原始大小写，供后续 sanitize_ident + SQL 拼接。
    // ③ 功能：判定映射表是否具备最基本的查询能力。
    let id_col = find_candidate(cols, ID_COL_CANDIDATES);
    Some(MappingTable { db_idx, table: table.to_string(), name_col, id_col })
    // ① 实现方式：id 列是**可选**条件（None 表示后面用隐式 rowid），随后打包成 MappingTable。
    // ② 关键程序解析：这里不校验 id 列的语义是否真是"消息表编号"，只看列名命中候选；
    //    因此一个普通的自增 `id` 列也会被接受，是模式 A 兜底可能路由错表的根源。
    // ③ 功能：产出可供 routing 使用的映射表描述。
}

/// 按候选序在列名中查找（大小写不敏感），返回数据库中的原始列名。
pub fn find_candidate(cols: &[String], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(hit) = cols.iter().find(|c| c.eq_ignore_ascii_case(cand)) {
            return Some(hit.clone());
        }
    }
    // ① 实现方式：外层按 candidates 顺序、内层在 cols 里做 eq_ignore_ascii_case 线性查找，首个命中即返回克隆。
    // ② 关键程序解析：**候选序优先于列序**——即使 cols 里靠后位置有个更高优先级的候选名，
    //    也会先被靠前的候选命中规则选中；返回原始大小写是为了后续 SQL 拼接与 schema 一致。
    //    复杂度 O(候选数 × 列数)，两者都是几十量级，可忽略。
    // ③ 功能：多候选列名匹配的统一实现（消息列、时间列、talker 列、映射列都走它）。
    None
    // ① 实现方式：全部候选落空返回 None。
    // ② 关键程序解析：调用方对 None 的处理各不相同——时间列 None 会退化成全表扫（不下推 LIMIT），
    //    talker 列 None 会关闭共享表模式，name 列 None 会放弃该映射表。
    // ③ 功能：给出"这套候选都不匹配"的信号，触发上层降级。
}

pub fn detect_time_column(cols: &[String]) -> Option<String> {
    find_candidate(cols, TIME_COL_CANDIDATES)
    // ① 实现方式：直接转调 find_candidate，套用 TIME_COL_CANDIDATES 候选序。
    // ② 关键程序解析：与 messages.rs 的时间列探测**不是同一套候选**（那边优先 sort_seq），
    //    本函数只服务统计/年度报告类时间轴；两者结果可能不同，不要互相替代。
    // ③ 功能：找出表里的创建时间列名。
}

#[cfg(test)]
mod tests {
    use super::msg_table_hash;

    #[test]
    fn msg_hash_matches_wechat4_md5_scheme() {
        // 微信 4.x 实测：消息表名后缀 = md5(username) 小写十六进制。
        assert_eq!(msg_table_hash("9000000001@chatroom"), "7d6d974f81e451e2c39dc66f5d3690fc");
        assert_eq!(msg_table_hash("wxid_fixture_1"), "03f539eac3ce8b4298e2ed633474ce04");
        // ① 实现方式：用两个**合成夹具** id（一个群聊形态、一个 wxid 形态）断言 md5 十六进制结果等于固定期望值。
        // ② 关键程序解析：期望值是离线算好的常量，钉住"小写 + 无分隔 + 原始字节参与哈希"三条规则；
        //    夹具 id 不是任何真实账号。群聊那条专门覆盖 `@chatroom` 后缀也参与哈希的行为。
        // ③ 功能：防止改动哈希口径（trim/大小写/编码）后主路由静默失效——那会表现为所有会话 0 条消息。
    }
}

/// SQL 标识符白名单校验（表名/列名无法参数化，拼接前必须过滤）。
pub fn sanitize_ident(name: &str) -> Result<String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#')
    {
        bail!("非法 SQL 标识符: {name}");
    }
    // ① 实现方式：非空 + 逐字节白名单 [A-Za-z0-9_$#]，任一不满足即 bail（错误串回显名字，不含密钥）。
    // ② 关键程序解析：表名/列名在 SQL 里不能用 ? 绑定，只能拼进双引号标识符，所以这道校验是注入防线；
    //    允许 `$`/`#` 是为了兼容 SQLite 合法标识符字符，但这使它比 db::table_columns 的白名单**更宽**，
    //    含 `$` 的表能过这里却在 table_columns 处失败。微信 4.x 的 `Msg_<md5hex>` 只含字母数字下划线。
    //    校验通过后名字里不可能有双引号/空格/分号，拼进 `"<name>"` 是安全的。
    // ③ 功能：把可拼接的 SQL 标识符收敛到安全集合。
    Ok(name.to_string())
    // ① 实现方式：原样返回自有 String（不做大小写转换、不加引号）。
    // ② 关键程序解析：加引号的责任在调用方的 format! 里；保留原始大小写确保与 schema 中的真实名字一致。
    // ③ 功能：交付已校验的标识符供 SQL 拼接。
}
