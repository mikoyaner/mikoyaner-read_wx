//! SQLCipher 账号上下文：连接打开/缓存、微信 4.x db_storage 布局发现、schema 内省。
//!
//! 真实库表结构在拿到 dbKey 前无法最终确认（任务 #7 用 <微信数据根> 验证），
//! 因此 schema 解析采取"运行时内省 + 多候选列名"策略，不做过度硬编码。

use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// db_storage 下各类库文件的位置。
#[derive(Debug, Clone, Default)]
pub struct DbLayout {
    pub session_db: PathBuf,
    // ① 实现方式：入口库路径原样保存（不做 canonicalize），后续 derive_account_dir / key_for 都基于它。
    // ② 关键程序解析：保留原始大小写很重要——keystore 的 rel_under_db_storage 用 Path::strip_prefix
    //    做**大小写敏感**的组件比较，一旦这里被规范化成别的大小写，每库密钥就查不中。
    // ③ 功能：账号上下文里唯一必填的库；Default 时是空 PathBuf（summary 会报 false）。
    /// message_0.db .. message_N.db，按序号升序。
    pub message_dbs: Vec<PathBuf>,
    /// media_0.db ..（媒体/硬链接元数据分库）。
    pub media_dbs: Vec<PathBuf>,
    // ① 实现方式：两个 Vec 由 numbered_dbs 按数字序号升序填充，空 Vec 表示"该类目没找到"。
    // ② 关键程序解析：顺序即 MsgIndex.dbs 的 db_idx 顺序，跨分库合并消息时按这个顺序遍历；
    //    用 Vec 而不是 Option 是因为一个账号可能有 0..N 个分库，且必须保序。
    // ③ 功能：消息正文分库清单 + 媒体元数据分库清单。
    pub contact_db: Option<PathBuf>,
    pub sns_db: Option<PathBuf>,
    pub emoticon_db: Option<PathBuf>,
    pub hardlink_db: Option<PathBuf>,
    pub message_fts_db: Option<PathBuf>,
    pub message_resource_db: Option<PathBuf>,
    // ① 实现方式：单文件类目一律 Option<PathBuf>，None = 该库在本机不存在（不是错误）。
    // ② 关键程序解析：这六个字段与 summary() 里的名字数组是**手工一一对应**的，
    //    新增字段忘了同步 summary，布局报告就会静默少一项；contact_db 额外有一层目录改名兜底。
    //    message_fts_db 指向虚表库（只能 LIKE，不能当普通表 SELECT 排序）。
    // ③ 功能：联系人 / 朋友圈 / 表情 / 硬链接 / 全文索引 / 资源库的定位结果。
}

impl DbLayout {
    /// 布局探测报告：每一项 =（名称，是否找到，说明）。
    /// 微信改目录/文件名的第一现场就在这里——先让人看见"哪个库没找到"，
    /// 而不是等到语音全 not_local、昵称全变空才怀疑。
    pub fn summary(&self) -> Vec<(String, bool, String)> {
        let count = |v: &Vec<PathBuf>| {
            if v.is_empty() {
                (false, "0 个".to_string())
            } else {
                (true, format!("{} 个：{}", v.len(),
                               v.iter().filter_map(|p| p.file_name()).map(|s| s.to_string_lossy().to_string())
                                .collect::<Vec<_>>().join(",")))
            }
        };
        // ① 实现方式：闭包把「分库 Vec」折成 (是否找到, 说明串)；说明串 = 个数 + 逗号拼接的文件名。
        // ② 关键程序解析：filter_map(file_name) 会丢掉没有文件名组件的路径（盘根/空 PathBuf），
        //    所以"3 个"后面可能只列出 2 个名字；to_string_lossy 保证非 UTF-8 文件名不会 panic。
        //    这里只看 Vec 长度、**不做 is_file() 复核**——路径在 discover 之后被微信删掉也仍报 true。
        // ③ 功能：message/media 这类多库项的一行式体检结果。
        let one = |o: &Option<PathBuf>| match o {
            Some(p) => (true, p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
            None => (false, "缺失".to_string()),
        };
        // ① 实现方式：Option 版折法，Some → (true, 文件名)，None → (false, "缺失")。
        // ② 关键程序解析：unwrap_or_default 让"路径无文件名"退化成空串而不是 panic；
        //    同样只判 Option 是否为 Some，不 stat 磁盘，与 session_db 那一项的口径不同。
        // ③ 功能：单库项的体检结果。
        let mut out = Vec::new();
        out.push(("session.db".into(), self.session_db.is_file(),
                  self.session_db.to_string_lossy().to_string()));
        let (m_ok, m_d) = count(&self.message_dbs);
        out.push(("message 分库".into(), m_ok, m_d));
        let (v_ok, v_d) = count(&self.media_dbs);
        out.push(("media 分库".into(), v_ok, v_d));
        // ① 实现方式：按固定顺序 push 三元组；session 项用 is_file() 真去 stat，并给出**完整路径**。
        // ② 关键程序解析：session 是唯一带全路径的项（便于人工核对账号目录），其余只给文件名，
        //    避免报告里到处是真实用户目录；is_file() 对"存在但无权限/是目录"都返回 false。
        // ③ 功能：报告的前三行——入口库 + 两类分库。
        for (name, opt) in [("contact.db", &self.contact_db), ("hardlink.db", &self.hardlink_db),
                            ("sns.db", &self.sns_db), ("emoticon.db", &self.emoticon_db),
                            ("message_fts.db", &self.message_fts_db),
                            ("message_resource.db", &self.message_resource_db)] {
            let (ok, detail) = one(opt);
            out.push((name.into(), ok, detail));
        }
        // ① 实现方式：遍历 (展示名, 字段引用) 的常量数组，统一交给 one() 折叠后 push。
        // ② 关键程序解析：数组顺序决定报告行序（contact 紧跟分库，因为它最容易因目录改名而缺失）；
        //    名字与字段是手工配对，改字段名/加字段必须同步这里，否则报告静默漏项。
        // ③ 功能：六个单库项的体检结果。
        out
        // ① 实现方式：返回 Vec<(String,bool,String)>，由上层 layout_json 直接转成 JSON 对象。
        // ② 关键程序解析：纯只读、无 I/O 副作用（除 session 那一次 stat），可在任何失败路径上安全调用。
        // ③ 功能：微信改目录/文件名时的第一现场证据。
    }
}

/// 从 session.db 路径反推账号根目录（.../wxid_xxx/db_storage/session/session.db）。
pub fn derive_account_dir(session_db: &Path) -> Option<PathBuf> {
    let mut dir = session_db.parent()?;
    // ① 实现方式：从文件路径剥掉最后一段得到所在目录（<账号目录>/db_storage/session）。
    // ② 关键程序解析：`?` 作用在 Option 上——传入裸文件名（parent 为 None）或空路径时直接返回 None，
    //    调用方据此退回"只有 session.db、没有账号目录"的降级布局。
    // ③ 功能：确定向上回溯的起点。
    loop {
        if dir.file_name()?.eq_ignore_ascii_case("db_storage") {
            return dir.parent().map(PathBuf::from);
        }
        // ① 实现方式：只比较路径最后一段的名字，用 eq_ignore_ascii_case 做大小写不敏感匹配。
        // ② 关键程序解析：命中后返回**它的上一级** = <账号目录>；file_name() 在盘根（如 `C:\`）
        //    返回 None，`?` 会就此终止整个函数 → None，所以循环不可能无限上溯。
        //    注意这里容忍 `DB_Storage` 这类大小写变体，但下游 keystore 的 strip_prefix 不容忍。
        // ③ 功能：认出 db_storage 这一层锚点。
        dir = dir.parent()?;
    }
    // ① 实现方式：逐级 parent() 上溯，直到命中锚点或走到路径根。
    // ② 关键程序解析：不依赖固定深度（不同版本 session 库可能在 session/ 或直接在 db_storage/ 下），
    //    也不做磁盘存在性校验——纯字符串层面的路径推导，零 I/O。
    // ③ 功能：由任意深度的 session.db 路径反推 <账号目录>，供每库密钥映射与 attach 子树定位使用。
}

fn numbered_dbs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: Vec<(i64, PathBuf)> = Vec::new();
    // ① 实现方式：暂存 (序号, 路径) 二元组，最后按序号排序再脱掉序号。
    // ② 关键程序解析：必须按**数字**排而不是文件名字典序，否则 message_10.db 会排在 message_2.db 前面，
    //    跨分库合并消息时的"分库顺序"就乱了；i64 而非 usize 是为了让 parse 直接吃下带符号的异常名。
    // ③ 功能：为按序号升序枚举分库做准备。
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // ① 实现方式：read_dir + flatten 遍历目录项，is_file() 过滤掉子目录，to_str() 取 UTF-8 文件名。
            // ② 关键程序解析：flatten 把单条 DirEntry 的 Err 静默跳过（权限/竞态删除不会中断整轮）；
            //    非 UTF-8 文件名（to_str 为 None）直接跳过——微信库名都是 ASCII，正常不会命中。
            // ③ 功能：拿到候选文件的原名，供后续前缀/后缀匹配。
            let lower = name.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix(prefix) else {
                continue;
            };
            let Some(rest) = rest.strip_suffix(".db") else {
                continue;
            };
            // ① 实现方式：先转小写，再双端 strip——去掉 `message_`/`media_` 前缀与 `.db` 后缀，剩下中段。
            // ② 关键程序解析：两次 let-else 保证只认「前缀 + 中段 + .db」这一种形状，
            //    因此 `message_0.db-wal`/`message_0.db-shm`/`message_fts.db` 都不会被误当分库
            //    （fts 的中段是 "fts"，会在下一步 parse 失败被丢弃）。prefix 由调用方保证已是小写。
            // ③ 功能：把文件名裁剪成纯序号串。
            if let Ok(idx) = rest.parse::<i64>() {
                out.push((idx, path));
            }
            // ① 实现方式：中段能 parse 成 i64 才收录，且 push 的是**原始大小写**的 path（不是 lower）。
            // ② 关键程序解析：这一步同时充当"排除非数字后缀"的过滤器；保留原路径是为了后续
            //    打开文件与密钥相对路径推导时和磁盘上的真实名字一致。
            // ③ 功能：只收集真正的编号分库。
        }
    }
    // ① 实现方式：整段遍历包在 `if let Ok(read_dir)` 里，目录不存在/无权限时**静默返回空 Vec**。
    // ② 关键程序解析：这是容错设计也是坑——目录被微信改名后这里不报错，
    //    上层只能从 DbLayout::summary 的"0 个"看出来，不会有任何 Err 冒泡。
    // ③ 功能：枚举失败时退化为"没有分库"，让 discover_layout 继续走兜底候选。
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, p)| p).collect()
    // ① 实现方式：sort_by_key 按序号升序（稳定排序），再 map 丢掉序号只留 PathBuf。
    // ② 关键程序解析：序号相同的两个文件（理论上不可能）会保持 read_dir 的原始相对顺序；
    //    返回值顺序直接成为 MsgIndex.dbs / db_idx 的顺序，构建后不可再变。
    // ③ 功能：产出按序号升序的分库路径清单。
}

fn find_db(db_dir: &Path, name: &str) -> Option<PathBuf> {
    let p = db_dir.join(name);
    p.is_file().then_some(p)
    // ① 实现方式：目录 join 文件名后用 is_file() 做一次 stat，bool::then_some 转成 Option。
    // ② 关键程序解析：is_file() 对"不存在 / 是目录 / 无权限"统一返回 false，因此调用方无法区分
    //    "缺失"和"读不到"；Windows 文件系统大小写不敏感，所以 `Media.db` 与 `media.db` 会命中同一个文件。
    // ③ 功能：单库定位的最小工具，缺失即 None（不报错）。
}

/// 以 session.db 为入口发现同账号的全部业务库。
pub fn discover_layout(session_db: &Path) -> DbLayout {
    let mut layout = DbLayout { session_db: session_db.to_path_buf(), ..Default::default() };
    let account_dir = match derive_account_dir(session_db) {
        Some(d) => d,
        None => return layout,
    };
    // ① 实现方式：先用入口路径填 session_db，其余字段走 Default（空 Vec / None）；再反推 <账号目录>。
    // ② 关键程序解析：反推失败时**提前返回只含 session 的布局**而不是报错——调用方仍能打开 session 库，
    //    只是消息/联系人全部为空，需要靠 summary() 看出"哪个库没找到"。
    // ③ 功能：建立布局骨架并确定扫描根。
    let storage = account_dir.join("db_storage");
    let msg_dir = storage.join("message");
    layout.message_dbs = numbered_dbs(&msg_dir, "message_");
    // ① 实现方式：固定拼 <账号目录>/db_storage/message，按 `message_` 前缀枚举编号分库。
    // ② 关键程序解析：这里用小写字面量 "db_storage" 拼接，而 derive_account_dir 是大小写不敏感匹配的；
    //    若磁盘上真叫 `DB_Storage`，Windows 仍能打开文件，但 keystore 的 strip_prefix 会因大小写不同而失配。
    // ③ 功能：定位微信 4.x 的消息分库目录。
    if layout.message_dbs.is_empty() {
        // 候选兜底：有的版本不分片（message.db）
        if let Some(p) = find_db(&msg_dir, "message.db") {
            layout.message_dbs.push(p);
        }
    }
    // ① 实现方式：编号分库为空时退一步找不分片的 message.db。
    // ② 关键程序解析：兜底只在"一个都没枚举到"时触发，不会和编号分库混用；
    //    仍找不到就保持空 Vec，由 MsgIndex::build 报"未发现 message_*.db 分库"。
    // ③ 功能：兼容不分片的旧布局。
    layout.media_dbs = numbered_dbs(&msg_dir, "media_");
    if layout.media_dbs.is_empty() {
        // 候选兜底：不分片的 media.db / Media.db（大小写不敏感文件系统上同名，仍列出备查）
        for name in ["media.db", "Media.db"] {
            if let Some(p) = find_db(&msg_dir, name) {
                layout.media_dbs.push(p);
                break;
            }
        }
    }
    // ① 实现方式：同一 message 目录下按 `media_` 前缀枚举；空则依次试 media.db / Media.db，命中即 break。
    // ② 关键程序解析：break 保证不会把同一个文件 push 两次（Windows 上两种写法指向同一文件）；
    //    media 分库缺失只影响媒体元数据，不影响消息正文读取。
    // ③ 功能：定位媒体/硬链接元数据分库。
    layout.message_fts_db = find_db(&msg_dir, "message_fts.db");
    layout.message_resource_db = find_db(&msg_dir, "message_resource.db");
    layout.contact_db = find_db(&storage.join("contact"), "contact.db");
    // ① 实现方式：三个单库直接按约定子目录 + 文件名定位。
    // ② 关键程序解析：message_fts.db 里是 FTS 虚表（只能 LIKE，不能 ORDER BY 普通列）；
    //    这三项都允许 None，不做兜底扫描（只有 contact 例外，见下）。
    // ③ 功能：全文索引库、资源库、联系人库定位。
    if layout.contact_db.is_none() {
        // contact 目录改名时，扫 db_storage 下一层找 contact.db（代价很小）
        layout.contact_db = ls_subdir_db(&storage, "contact.db");
    }
    // ① 实现方式：contact 缺失时降级为"扫 db_storage 的直接子目录"找同名库。
    // ② 关键程序解析：只扫一层，代价 O(子目录数)；联系人库是昵称/群成员的唯一来源，
    //    它缺失会让所有昵称变空，所以值得单独兜底，而 sns/emoticon 缺了只影响次要功能。
    // ③ 功能：抗 contact 目录改名。
    layout.sns_db = find_db(&storage.join("sns"), "sns.db");
    layout.emoticon_db = find_db(&storage.join("emoticon"), "emoticon.db");
    layout.hardlink_db = find_db(&storage.join("hardlink"), "hardlink.db");
    layout
    // ① 实现方式：补齐朋友圈/表情/硬链接三个单库后返回整个布局。
    // ② 关键程序解析：全程只做 read_dir/is_file 这类**只读**文件系统探测，不打开任何数据库、不写盘；
    //    hardlink.db 是图片 md5 → attach 子树映射的来源，缺失时媒体定位只能靠扫目录。
    // ③ 功能：产出该账号的完整库位置清单（找不到的项保持 None/空）。
}

/// 在 `root` 的直接子目录里找名为 `name` 的库（只扫一层，防目录重命名后彻底静默）。
fn ls_subdir_db(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    // ① 实现方式：read_dir 的 Err 用 .ok()? 转成 None 直接结束。
    // ② 关键程序解析：root 不存在/无权限时不报错、不 panic，调用方只会看到"该库缺失"。
    // ③ 功能：打开一层目录枚举。
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let cand = p.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // ① 实现方式：只看 root 的**直接子目录**，逐个 join 目标文件名并 is_file() 试命中，首个命中即返回。
    // ② 关键程序解析：不递归（防目录树爆炸/防扫到别的账号），命中顺序取决于 read_dir 的返回顺序，
    //    因此多个子目录里都有同名库时结果不确定——只在约定目录失效后的兜底场景使用。
    // ③ 功能：抗子目录改名（如 contact → contact_v2）。
    None
    // ① 实现方式：遍历完没命中返回 None。
    // ② 关键程序解析：与 find_db 的 None 语义一致，上层不区分"没扫到"和"目录打不开"。
    // ③ 功能：兜底失败的统一出口。
}

/// 用 SQLCipher hex 密钥打开数据库并验证可读。
pub fn open_sqlcipher(path: &Path, key_hex: &str) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    // ① 实现方式：rusqlite 0.32.1 的 `Connection::open` = `OpenFlags::default()`，即
    //    READ_WRITE | CREATE | NO_MUTEX | URI；内部还默认调了 sqlite3_busy_timeout(5000)（5 秒忙等）。
    //    错误用 anyhow 包一层，带上 path.display() 便于定位是哪个库打不开。
    // ② 关键程序解析（**已知待用户决策的只读口径问题，不要顺手改这行**）：
    //    - CREATE：路径不存在时会在微信数据目录里**新建一个空库**；READ_WRITE：没有 PRAGMA query_only，
    //      连接本身具备写权限（本模块所有 SQL 都是 SELECT/PRAGMA，但权限口径不是只读）。
    //    - WAL 可见性：读写连接能正常读到未 checkpoint 的 -wal 内容；改成 READ_ONLY 后，
    //      若 -shm 不存在且目录不可写，SQLite 会直接 SQLITE_CANTOPEN——这正是要先实测的点。
    //    - 副作用面：打开/关闭 WAL 库可能创建或写 -shm、触发 WAL recovery；最后一个连接关闭时
    //      可能触发 checkpoint 回写 .db。是否改只读必须先实测 .db/-wal 的字节与 mtime 变化。
    //    - URI flag：路径若以 `file:` 开头会被当 URI 解析（含 ?/# 时语义变化）；NO_MUTEX 表示
    //      串行化交给上层 Account 的 Mutex，而不是 SQLite 自己的连接锁。
    //    - 5 秒忙等：微信 WCDB 正在 checkpoint/写事务时，这里最多等 5s 后返回 SQLITE_BUSY。
    // ③ 功能：拿到一个指向目标 .db 的连接句柄（此时还未设密钥，读到的是加密页）。
    let key = key_hex.trim();
    // 只接受 hex 密钥（WeFlow 约定：64 位 hex 字符串）；PRAGMA 值必须原样拼接，
    // 因此先做严格白名单校验，杜绝 SQL 注入。
    if key.is_empty()
        || key.len() % 2 != 0
        || !key.bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("dbKey 不是合法的十六进制字符串");
    }
    // ① 实现方式：trim 后做三条白名单校验——非空、偶数长度、逐字节 is_ascii_hexdigit，任一不满足即 bail。
    // ② 关键程序解析：这是 PRAGMA key 唯一的注入防线（PRAGMA 值不能用 ? 绑定，只能字符串拼接），
    //    校验通过后 key 里不可能出现引号/分号/空格，拼进 SQL 是安全的。
    //    注意校验**没有强制长度 == 64**：注释说的是约定，代码只要求偶数长度 hex；
    //    非 32 字节的 blob key 会被 SQLCipher 当 passphrase 走 PBKDF2（慢且必然打不开），
    //    错误最终表现为下面的"密钥错误或数据库损坏"，而不是这里的快速失败。
    //    错误串里不回显 key 本身，避免明文密钥进日志。
    // ③ 功能：把密钥收敛到"可安全拼接的 hex"集合。
    conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))?;
    // ① 实现方式：`PRAGMA key = "x'<hex>"` —— 外层双引号是 SQL 字符串，内层 x'...' 是 blob 字面量，
    //    SQLCipher 识别为 32 字节 raw key（跳过 passphrase 的 PBKDF2 派生）。execute_batch 一次执行完。
    // ② 关键程序解析：必须在**任何读表之前**设 key，否则第一次读页就会以"无密钥"状态判定文件头失败；
    //    这里用 `?` 直接冒泡 rusqlite 错误（errmsg 不含 key 内容）。key 是上一页校验过的纯 hex。
    // ③ 功能：给连接装上 SQLCipher 解密密钥。
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|e| anyhow!("密钥错误或数据库损坏: {e}"))?;
    // ① 实现方式：设完 key 立刻读一次 sqlite_master 计数，强制 SQLite 真正解密并解析第 1 页。
    // ② 关键程序解析：PRAGMA key 本身**不校验密钥正确性**（它只是记下 key），错误密钥要到首次读页
    //    才暴露成 "file is not a database"；这一步就是那个"显式验钥"动作。闭包丢弃计数值只关心成功与否。
    //    错误被统一改写成中文提示，原始 errmsg 附在后面（不含密钥）。
    // ③ 功能：确认「密钥正确且库可读」，把失败拦在 Account::open 阶段而不是后续查询里。
    Ok(conn)
    // ① 实现方式：返回已验钥的连接所有权，由调用方（Account::conn）包进 Arc<Mutex<>> 缓存。
    // ② 关键程序解析：没有显式 close，连接寿命 = Account 寿命；Drop 时 rusqlite 调 sqlite3_close_v2。
    // ③ 功能：交付一个可用的只读用途句柄。
}

pub fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    // ① 实现方式：查 sqlite_master 里 type='table' 的 name，按 name 升序（BINARY collation，区分大小写）。
    // ② 关键程序解析：type='table' **同时包含 FTS 虚表本体**（虚表在 sqlite_master 里也登记为 'table'），
    //    只排除 view/index/trigger；ASCII 序下大写 `Msg_*` 排在小写 `message_*` 之前，
    //    这个顺序决定 MsgIndex::build 抽样哪张表去探 talker 列。纯静态 SQL，无拼接、无注入面。
    // ③ 功能：列出库里所有表名，供 schema 内省。
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // ① 实现方式：query_map 逐行取第 0 列成 String，collect 成 Result<Vec<_>> 后 `?` 一次性冒泡任一行错误。
    // ② 关键程序解析：collect::<Result<Vec,_>>() 是"全成功才返回"语义，中途某行解码失败整轮失败；
    //    必须写全 std::result::Result，因为本文件的 Result 已被 anyhow 别名占用。
    // ③ 功能：把表名行集物化成 Vec<String>（stmt 随后 Drop，不长期占用读游标）。
    Ok(rows)
    // ① 实现方式：直接返回表名向量。
    // ② 关键程序解析：空库返回空 Vec 而不是 Err，"没有消息分表"的判定留给 MsgIndex::build。
    // ③ 功能：交付内省结果。
}

pub fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    // 表名无法参数化，先白名单校验再拼接
    if table.is_empty() || !table.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("非法表名: {table}");
    }
    // ① 实现方式：表名白名单 = 非空 + 仅 [A-Za-z0-9_]，不满足直接 bail（错误串回显表名，不含密钥）。
    // ② 关键程序解析：PRAGMA 的参数不能用 ? 绑定、只能拼接，所以这道校验就是注入防线；
    //    它比 schema::sanitize_ident **更严**（后者还允许 `$`/`#`），含 `$` 的表能通过
    //    stream_table 的标识符校验却在这里失败 → 表现为"列取不到"。微信 4.x 的 `Msg_<md5hex>` 不受影响。
    // ③ 功能：把可拼接的表名收敛到安全集合。
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // ① 实现方式：上一行 prepare 的是 `PRAGMA table_info("<table>")`（表名双引号包成标识符），
    //    这里 query_map 取每行索引 1（name 字段；索引 0 是 cid），collect 成 Result<Vec> 后 `?`。
    // ② 关键程序解析：表不存在时 PRAGMA **不报错、返回 0 行**，于是得到空列名 Vec；
    //    上层 find_candidate 全落空 → 认定无时间列 → 不下推 ORDER BY/LIMIT，静默退化成全表扫。
    //    列名保持数据库原始大小写（不做 lower），因为后续要原样拼进 `ORDER BY "<col>"`；
    //    大小写归一只发生在 find_candidate 的比较阶段。
    // ③ 功能：产出列名清单，供多候选列名匹配与 SQL 拼接。
    Ok(cols)
    // ① 实现方式：返回列名向量（可能为空）。
    // ② 关键程序解析：结果会被 Account::columns 按 (db,table) 缓存，**空 Vec 也会被缓存**，
    //    所以"表刚被微信建出来"这类时序问题要靠 invalidate_msg_index 才能重试。
    // ③ 功能：交付内省结果。
}

/// 一个账号的全部状态：会话连接、按需缓存的分库连接、我的账号 ID。
pub struct Account {
    pub layout: DbLayout,
    pub key_hex: String,
    pub account_dir: Option<PathBuf>,
    pub keystore: crate::keystore::KeyStore,
    // ① 实现方式：四个不可变（打开后不再改）的上下文字段；key_hex 是主 session 密钥的**明文** String。
    // ② 关键程序解析：Account **故意不 derive Debug**，避免 `{:?}` 把 key_hex 整串打进日志；
    //    keystore 是 open 时的一次性快照（load_default），进程运行期间 key-provider 新写入的密钥
    //    不会被这里看到，必须重开账号才生效。account_dir 为 None 时每库密钥映射整体失效、只用主 key。
    // ③ 功能：承载"布局 + 主密钥 + 账号目录 + 每库密钥库"四件套。
    pub my_account_id: Mutex<Option<String>>,
    // ① 实现方式：Mutex<Option<String>>，由上层 ABI（wcdb_set_my_account_id / welive 请求字段）后置写入。
    // ② 关键程序解析：None 时 messages::normalize_row 无法判定 computed_is_send，只能回退 is_send 列或 Null；
    //    用 Mutex 是因为 Arc<Account> 会被多线程共享而该字段要可变。读侧一律 .lock().ok() 容忍 poisoned。
    // ③ 功能：记录"我是谁"，用于区分自己发出的消息与对方消息。
    conns: Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>,
    columns_cache: Mutex<HashMap<(PathBuf, String), Arc<Vec<String>>>>,
    msg_index: Mutex<Option<Arc<crate::schema::MsgIndex>>>,
    // ① 实现方式：三层私有缓存，全部 Mutex 保护、全部懒填充。conns 以**库路径**为键缓存连接；
    //    columns_cache 以 (库路径, 表名) 为键缓存列名；msg_index 缓存整个分表索引。
    // ② 关键程序解析：Arc<Mutex<Connection>> 是双层结构——外层 Arc 让调用方能把句柄拿走，
    //    内层 Mutex 串行化同一个库上的所有 SQL（rusqlite 以 NO_MUTEX 打开，SQLite 自身不再加锁）。
    //    连接一旦建立就活到 Account Drop，没有超时/LRU/显式 close；列名与索引缓存也没有 TTL，
    //    微信新建分表后必须显式 invalidate_msg_index 才能看见。值用 Arc 是为了命中缓存时零拷贝返回。
    // ③ 功能：避免重复开库、重复 PRAGMA table_info、重复扫全部分表。
}

impl Account {
    /// 打开账号（session.db + 密钥），失败原因由调用方映射为状态码。
    pub fn open(session_db_path: &str, key_hex: &str) -> Result<Arc<Account>> {
        let path = PathBuf::from(session_db_path);
        if !path.is_file() {
            bail!("session.db 不存在: {session_db_path}");
        }
        // ① 实现方式：字符串转 PathBuf 后先 is_file() 做一次存在性检查，不存在直接 bail。
        // ② 关键程序解析：这一步很关键——open_sqlcipher 用的是带 CREATE 标志的 open，
        //    若不先拦住"路径不存在"，就会在微信数据目录里凭空建出一个空库。
        //    错误串回显调用方传入的原始路径（便于定位账号目录），不含密钥。
        // ③ 功能：把"账号入口库缺失"变成明确失败，而不是静默创建。
        let layout = discover_layout(&path);
        let account_dir = derive_account_dir(&path);
        let keystore = crate::keystore::KeyStore::load_default();
        // ① 实现方式：依次做布局探测、账号目录反推、密钥库加载（三次独立只读操作）。
        // ② 关键程序解析：discover_layout 内部会再调一次 derive_account_dir（重复推导，纯路径运算无副作用）；
        //    load_default 读的是 store_path() 解析出的那份 JSON，读失败/解析失败都静默返回空库，
        //    于是"密钥库坏了"在这里表现为"所有分库都回退主 key、然后打不开"。
        // ③ 功能：把账号运行所需的三份只读上下文一次性备齐。
        let account = Arc::new(Account {
            layout,
            key_hex: key_hex.to_string(),
            account_dir,
            keystore,
            my_account_id: Mutex::new(None),
            conns: Mutex::new(HashMap::new()),
            columns_cache: Mutex::new(HashMap::new()),
            msg_index: Mutex::new(None),
        });
        // ① 实现方式：把上下文装进 Arc<Account>，三个缓存初始化为空、my_account_id 初始化为 None。
        // ② 关键程序解析：返回 Arc 而非 Account 是因为上层 ABI 要把句柄长期持有并跨线程共享；
        //    key_hex 在这里被克隆成自有 String，调用方传入的 &str 生命周期不再相关。
        // ③ 功能：构造账号对象（此时还没有任何数据库连接）。
        // 立即打开 session 库验证密钥
        account.conn(&path)?;
        // ① 实现方式：主动调一次 conn(session.db)，触发 key_for → open_sqlcipher → 读 sqlite_master 验钥。
        // ② 关键程序解析：`?` 让验钥失败直接冒泡，Account 对象随即被 Drop（缓存里那条连接也一起关闭）；
        //    注意 conn 内部走的是 keystore 优先解析，若每库表里登记了**错误的** session key，
        //    即使调用方传入的主 key 正确，这里也会失败。
        // ③ 功能：把"密钥错误"暴露在 open 阶段，而不是等到第一次读消息。
        Ok(account)
        // ① 实现方式：返回已验钥的账号句柄。
        // ② 关键程序解析：session 连接此时已进入 conns 缓存，后续 session_conn() 直接命中、不再重复验钥。
        // ③ 功能：交付可用的账号上下文。
    }

    pub fn account_dir(&self) -> Option<PathBuf> {
        self.account_dir.clone()
        // ① 实现方式：克隆内部 Option<PathBuf> 返回副本。
        // ② 关键程序解析：返回自有值而非引用，避免把 &self 的生命周期带给调用方（ABI 层要转成 C 字符串）；
        //    None 表示入口路径里没找到 db_storage 锚点。
        // ③ 功能：对外暴露 <账号目录>，供 attach 子树定位与图片密钥桶命名使用。
    }

    /// 该库应使用的密钥：每库表命中则用之，否则回退主 key。
    fn key_for(&self, path: &Path) -> String {
        self.keystore.resolve_key(path, self.account_dir.as_deref(), &self.key_hex)
        // ① 实现方式：委托 keystore::resolve_key，按 (账号目录规范化串, db_storage 下相对路径) 查每库密钥表。
        // ② 关键程序解析：as_deref 把 Option<PathBuf> 转成 Option<&Path>；查不中就用主 key 兜底
        //    （兼容微信 3.x / 单密钥场景）。返回值是明文密钥 String，只在本函数与 open_sqlcipher 之间流动，
        //    绝不进日志、绝不写回任何数据库。
        // ③ 功能：实现"每库一钥"的密钥选择。
    }

    /// 取（或建立）到指定库文件的连接句柄。连接按路径缓存，Mutex 串行化访问。
    pub fn conn(&self, path: &Path) -> Result<Arc<Mutex<Connection>>> {
        let mut cache = self
            .conns
            .lock()
            .map_err(|_| anyhow!("connection cache poisoned"))?;
        // ① 实现方式：先锁住整张连接缓存表；poisoned（有线程在持锁期间 panic）时映射成 anyhow 错误。
        // ② 关键程序解析：这把锁在**整个开库过程中**一直被持有（下面 open_sqlcipher 也在锁内），
        //    所以并发首次访问不同库会被串行化；好处是不会对同一路径重复开连接，代价是开库慢时全体等待。
        // ③ 功能：取得缓存表的独占访问。
        if let Some(c) = cache.get(path) {
            return Ok(Arc::clone(c));
        }
        // ① 实现方式：以 PathBuf 为键查缓存，命中就 Arc::clone 返回（只加引用计数，不复制连接）。
        // ② 关键程序解析：键是**未经规范化**的路径，大小写/斜杠写法不同的同一文件会被当成两个键、
        //    开出两条连接；本模块内部路径都来自同一份 layout，所以写法一致。
        // ③ 功能：连接复用，避免重复 PRAGMA key 与重复验钥。
        let key = self.key_for(path);
        let conn = open_sqlcipher(path, &key)?;
        let c = Arc::new(Mutex::new(conn));
        cache.insert(path.to_path_buf(), Arc::clone(&c));
        Ok(c)
        // ① 实现方式：未命中则解析该库密钥 → open_sqlcipher 开库并验钥 → 包 Arc<Mutex<>> → 写回缓存 → 返回。
        // ② 关键程序解析：open_sqlcipher 失败时 `?` 直接返回，**不会往缓存里塞坏连接**，下次调用可重试；
        //    成功后缓存与调用方各持一个 Arc，连接寿命 = 两者中较长的那个（通常是 Account 整体 Drop）。
        //    没有空闲回收/超时关闭，长期持有会让微信侧的 -shm 一直存在。
        // ③ 功能：按需建立并缓存到任意分库的连接。
    }

    /// 在指定库上执行一段只读/写入逻辑，自动加解锁。
    pub fn with_conn<T>(&self, path: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let arc = self.conn(path)?;
        let conn = arc.lock().map_err(|_| anyhow!("connection poisoned"))?;
        // ① 实现方式：先取（或建）连接 Arc，再锁内层 Mutex 拿到 &Connection，poisoned 映射成 anyhow 错误。
        // ② 关键程序解析：两把锁的获取顺序固定为「缓存表锁 → 连接锁」，且 conn() 返回时缓存表锁已释放，
        //    因此不会出现锁序反转；但内层 Mutex 不可重入——若 f 内部再对**同一个库**调 with_conn 会死锁。
        // ③ 功能：把"取连接 + 加锁"收成一步，保证同一库上的 SQL 串行执行。
        f(&conn)
        // ① 实现方式：把 &Connection 交给回调，回调结果原样返回；guard 在函数结束时 Drop 自动解锁。
        // ② 关键程序解析：锁在整个回调期间持有——流式读消息时回调可能执行很久（写 JSONL），
        //    这段时间该库上的其它查询全部阻塞；回调里抛错也会正常解锁（RAII），不会把连接锁死。
        // ③ 功能：在受保护的连接上执行一段业务逻辑（本模块内一律是 SELECT/PRAGMA 只读语句）。
    }

    pub fn session_conn(&self) -> Result<Arc<Mutex<Connection>>> {
        self.conn(&self.layout.session_db)
        // ① 实现方式：把 layout.session_db 直接喂给 conn()，复用同一套缓存与验钥逻辑。
        // ② 关键程序解析：Account::open 已经预热过这条连接，所以这里几乎总是缓存命中；
        //    返回的是 Arc<Mutex<>>，调用方需自己 lock 才能执行 SQL。
        // ③ 功能：给会话列表/联系人路由等 session 库查询提供入口句柄。
    }

    /// PRAGMA table_info 结果缓存（列名保持数据库原始大小写）。
    pub fn columns(&self, db: &Path, table: &str) -> Result<Arc<Vec<String>>> {
        let key = (db.to_path_buf(), table.to_string());
        if let Ok(cache) = self.columns_cache.lock() {
            if let Some(hit) = cache.get(&key) {
                return Ok(Arc::clone(hit));
            }
        }
        // ① 实现方式：以 (库路径, 表名) 二元组为键查缓存，命中则 Arc::clone 返回。
        // ② 关键程序解析：用 `if let Ok(lock)` 而不是 `?`——缓存锁 poisoned 时**降级为不缓存**继续往下走，
        //    不影响功能正确性；键里的表名保留原始大小写，同一张表写成不同大小写会各缓存一份。
        // ③ 功能：避免每张分表每次读消息都重复 PRAGMA table_info。
        let cols = self.with_conn(db, |conn| table_columns(conn, table))?;
        let arc = Arc::new(cols);
        // ① 实现方式：未命中则在该库连接上执行 table_columns，结果包成 Arc<Vec<String>>。
        // ② 关键程序解析：`?` 会把"非法表名/连接打不开"冒泡给调用方；注意**表不存在时不报错**，
        //    得到的是空 Vec，也会被当成有效结果缓存下来。Arc 让多个调用方共享同一份列名而不克隆。
        // ③ 功能：真正做一次列内省。
        if let Ok(mut cache) = self.columns_cache.lock() {
            cache.insert(key, Arc::clone(&arc));
        }
        // ① 实现方式：再锁一次缓存表写入结果（读锁与写锁之间不持锁，避免在锁内做 I/O）。
        // ② 关键程序解析：两次 lock 之间存在窗口，并发时可能重复内省同一张表，但结果一致、无副作用；
        //    poisoned 时静默跳过写入，下次仍会重新内省。
        // ③ 功能：填充缓存。
        Ok(arc)
        // ① 实现方式：返回本次构建（或命中）的 Arc。
        // ② 关键程序解析：缓存无 TTL，微信新增列/新增表后需要 invalidate_msg_index 清空才会重新内省。
        // ③ 功能：交付列名清单。
    }

    /// 消息库分表索引（懒构建，失败不缓存以便下次重试）。
    pub fn msg_index(&self) -> Result<Arc<crate::schema::MsgIndex>> {
        if let Ok(cache) = self.msg_index.lock() {
            if let Some(hit) = cache.as_ref() {
                return Ok(Arc::clone(hit));
            }
        }
        // ① 实现方式：Option<Arc<MsgIndex>> 型缓存，Some 即命中，Arc::clone 零拷贝返回。
        // ② 关键程序解析：与 columns 一样用 `if let Ok(lock)` 容忍 poisoned；锁在这段结束后立刻释放，
        //    不会在持锁期间去扫库（扫库要开 N 个连接、耗时可达秒级）。
        // ③ 功能：让同一账号的分表索引只构建一次。
        let index = crate::schema::MsgIndex::build(self)?;
        // ① 实现方式：未命中则调 MsgIndex::build 遍历所有 message 分库做内省。
        // ② 关键程序解析：`?` 直接冒泡失败（如"未发现 message_*.db 分库"），**失败路径不写缓存**，
        //    所以下次调用会重试——这正是注释里"失败不缓存"的含义；成功前并发调用可能重复构建。
        // ③ 功能：建立 db_idx → 表清单、消息分表集合、映射表/talker 列的路由索引。
        if let Ok(mut cache) = self.msg_index.lock() {
            *cache = Some(Arc::clone(&index));
        }
        // ① 实现方式：再锁一次把 Some(Arc) 写回缓存槽。
        // ② 关键程序解析：poisoned 时静默跳过（功能不受影响，只是每次都重建）；
        //    写入后除非显式 invalidate_msg_index，索引终身不变——微信为**新会话**新建 Msg_<md5> 表时，
        //    长寿命句柄会一直看不到它，路由退化成 "none" 并静默返回 0 条。
        // ③ 功能：缓存索引。
        Ok(index)
        // ① 实现方式：返回本次构建的 Arc（与缓存里那份指向同一对象）。
        // ② 关键程序解析：调用方拿到的是快照，之后 invalidate 也不会影响已持有的 Arc。
        // ③ 功能：交付分表索引。
    }

    /// 消息库布局变化（新增分库/分表）后重建索引。
    pub fn invalidate_msg_index(&self) {
        if let Ok(mut cache) = self.msg_index.lock() {
            *cache = None;
        }
        // ① 实现方式：把索引槽置回 None，下次 msg_index() 会重新 build。
        // ② 关键程序解析：只清索引、**不关闭 conns 里的连接**，所以已建立的分库连接继续复用；
        //    已经发出去的 Arc<MsgIndex> 快照不受影响（调用方需重新取）。
        // ③ 功能：让新增分表/新增分库可见。
        if let Ok(mut cache) = self.columns_cache.lock() {
            cache.clear();
        }
        // ① 实现方式：连带清空 (库,表) → 列名 的全部缓存。
        // ② 关键程序解析：必须一起清——新表可能列结构不同，且此前"表不存在"时缓存的空列名 Vec
        //    会让新表永远被判定为无时间列；两把锁分别获取、不在同一临界区，无锁序问题。
        // ③ 功能：保证重建索引后列内省也是新鲜的。
    }

    /// wcdb_purge_memory：释放页缓存，不关闭活动连接、不影响游标。
    pub fn shrink_memory(&self) {
        if let Ok(cache) = self.conns.lock() {
            for conn in cache.values() {
                if let Ok(c) = conn.lock() {
                    let _ = c.execute_batch("PRAGMA shrink_memory;");
                    // ① 实现方式：对每条已缓存连接执行 `PRAGMA shrink_memory`，让 SQLite 尽量归还页缓存内存。
                    // ② 关键程序解析：`let _ =` 吞掉错误（某库正忙/锁住也不影响其它库）；
                    //    逐条 lock 内层 Mutex，若某条连接正被流式读取占用，这里会**阻塞等待**它读完；
                    //    该 PRAGMA 不关闭连接、不中断已打开的游标，也不写数据库文件。
                    // ③ 功能：响应上层的内存回收请求，压低长期持有多个 SQLCipher 连接的常驻内存。
                }
            }
        }
        // ① 实现方式：外层锁缓存表、遍历所有连接；两层锁都用 `if let Ok` 容忍 poisoned。
        // ② 关键程序解析：只处理**已建立**的连接，不会为了 shrink 去开新库；
        //    整个过程无返回值、无错误冒泡，属"尽力而为"的维护操作。
        // ③ 功能：批量释放页缓存。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_account_dir_walks_up() {
        let p = Path::new(r"C:\wechat_data\wxid_abc_1234\db_storage\session\session.db");
        assert_eq!(
            derive_account_dir(p),
            Some(PathBuf::from(r"C:\wechat_data\wxid_abc_1234"))
        );
        // ① 实现方式：构造一条 <账号目录>/db_storage/session/session.db 形状的路径（纯字符串，不碰磁盘），
        //    断言 derive_account_dir 逐级上溯后正好返回 <账号目录>。
        // ② 关键程序解析：夹具路径是合成的占位账号，不是任何真实用户目录；断言同时验证了
        //    "命中 db_storage 就停、返回其上一级"这条边界，多一层少一层都会失败。
        // ③ 功能：锁住账号目录反推规则，防后续改路径解析时静默偏移一层。
    }

    #[test]
    fn rejects_non_hex_key() {
        let tmp = std::env::temp_dir().join("weflow_core_test_nonhex.db");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, b"not a db").unwrap();
        // ① 实现方式：在系统 temp_dir 下造一个内容为 "not a db" 的假库文件，先删后写保证可重复运行。
        // ② 关键程序解析：只写 temp_dir，**绝不碰微信数据目录**；必须先把文件写出来，
        //    否则 open_sqlcipher 的 CREATE 标志会自己建一个空库，测的就不是"非法密钥拒绝"了。
        // ③ 功能：准备一个必然打不开的靶子文件。
        assert!(open_sqlcipher(&tmp, "zz12").is_err());
        assert!(open_sqlcipher(&tmp, "").is_err());
        assert!(open_sqlcipher(&tmp, "0x1234").is_err());
        let _ = std::fs::remove_file(&tmp);
        // ① 实现方式：三种非法密钥各断言一次 is_err——非 hex 字符、空串、带 `0x` 前缀（x 不是 hex 数字）。
        // ② 关键程序解析：覆盖的是 PRAGMA key 拼接前的白名单分支；"0x1234" 这条专门盯住
        //    "看起来像十六进制但含前缀"的输入，防止有人放宽校验后把前缀一起拼进 SQL。
        //    末尾 remove_file 用 `let _` 忽略失败，避免清理动作影响断言结论。
        // ③ 功能：锁住密钥白名单，保证非法 key 在拼 SQL 之前就被拒。
    }
}
