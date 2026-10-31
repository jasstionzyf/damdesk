//! L2 `svc-import::path_tags` — D8 path 标签：目录名/文件名解析为标签（用户可选开关）。
//!
//! 裁决 2026-10-13：settings `path_tags: {useDirs, useFilenames}` 默认均 false。
//! 规则（PLAN.md M1 §path 标签）：
//! ① 目录取相对导入根的路径段（根本身不入），父链每段保留；
//! ② 文件名去扩展名后丢弃相机/系统噪声（IMG_/DSC_/Screenshot/微信图片_/纯数字段/副本/copy 等），
//!    剩余按分隔符（空格 _ - （）·）切词，英文归一小写、全半角统一；
//! ③ 中文无分隔符段不内切词，整段作标签；
//! ④ 单个标签 token 超过 6 字符丢弃（目录段与文件名词同样适用；恰好 6 字保留）；
//! ⑤ 过滤后为空不产标签。
//! 标签入 tag 表 origin='path'（与 auto/manual 共存不合并：同名已存在则让位跳过，
//! 不改写既有行的 origin）；入库前规范化去重。
//! 开关关闭：导入/watcher 不产标签、不回删已入库标签；设置页重扫按当前开关全量重建。

use std::path::{Component, Path, PathBuf};

use core_config::{PathTagSettings, PATH_TAG_SETTINGS_KEY};
use rusqlite::Connection;

/// 重扫结果（设置页按钮返回）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, PartialEq, Eq)]
pub struct PathTagRescanResult {
    pub assets: usize,
    pub inserted: usize,
    pub removed: usize,
}

/// 当前 path 标签开关（settings 表缺失 → 默认全关）。
pub fn settings(conn: &Connection) -> PathTagSettings {
    core_storage::settings::get_or_default(conn, PATH_TAG_SETTINGS_KEY).unwrap_or_default()
}

// ---------- 纯解析（表驱动测试锁死） ----------

/// 全半角统一 + 英文归一：全角 ASCII 区（！..～，含字母/数字/括号/下划线/横线）→ 半角，
/// 全角空格 → 空格，片假名中点 → ·，再统一小写。
fn width_lower(c: char) -> char {
    let half = match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        '\u{30FB}' => '·',
        _ => c,
    };
    half.to_lowercase().next().unwrap_or(half)
}

/// 分隔符（空格/制表/下划线/横线/圆括号/间隔号，含全角变体）。
fn is_sep(c: char) -> bool {
    matches!(c, ' ' | '\t' | '_' | '-' | '(' | ')' | '·' | '・' | '　')
}

/// 相机/系统噪声：整 token 命中即丢弃（小写比较）。
const NOISE_EXACT: &[&str] = &[
    "img",
    "dsc",
    "dscn",
    "vid",
    "screenshot",
    "mmexport",
    "微信图片",
    "副本",
    "copy",
];

/// 噪声前缀：前缀 + 纯数字（IMG1234 / DSC0123 / 微信图片20240101 / 副本2）。
const NOISE_PREFIX_DIGITS: &[&str] = &[
    "img",
    "dsc",
    "dscn",
    "vid",
    "mmexport",
    "screenshot",
    "微信图片",
    "副本",
];

fn is_noise(tok: &str) -> bool {
    if tok.is_empty() {
        return true;
    }
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return true; // 纯数字段（含 "(1)" 切出的 "1"、"2024"）
    }
    if NOISE_EXACT.contains(&tok) {
        return true;
    }
    NOISE_PREFIX_DIGITS
        .iter()
        .any(|p| tok.starts_with(p) && tok[p.len()..].chars().all(|c| c.is_ascii_digit()))
}

/// 单个标签 token 上限（裁决 2026-10-13：超 6 字符丢弃防长句废标签；恰好 6 保留）。
pub const MAX_TOKEN_CHARS: usize = 6;

/// 单个名字（目录段或文件名词干）→ 规范化标签 token 列表（保序去重）。
pub fn parse_name(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            let tok = std::mem::take(cur);
            if !is_noise(&tok) && tok.chars().count() <= MAX_TOKEN_CHARS && !out.contains(&tok) {
                out.push(tok);
            }
        }
    };
    for c in raw.chars() {
        let c = width_lower(c);
        if is_sep(c) {
            flush(&mut cur, &mut out);
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// 目录段标签：path 相对 root 的每一段（根本身不入，父链每段保留；末段=文件名跳过）。
pub fn dir_tags(cfg: &PathTagSettings, root: Option<&Path>, path: &Path) -> Vec<String> {
    if !cfg.use_dirs {
        return vec![];
    }
    let Some(r) = root else { return vec![] };
    let Ok(rel) = path.strip_prefix(r) else {
        return vec![];
    };
    let segs: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    for seg in segs.iter().take(segs.len().saturating_sub(1)) {
        for t in parse_name(seg) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// 文件名词干标签。
pub fn stem_tags(cfg: &PathTagSettings, file_name: &str) -> Vec<String> {
    if !cfg.use_filenames {
        return vec![];
    }
    // fileName 列含扩展名：去最后一个扩展（与 Path::file_stem 同语义）
    let stem = match file_name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => file_name,
    };
    parse_name(stem)
}

/// 完整推导（目录段 + 文件名词干，合并保序去重）。开关全关返回空。
pub fn derive_tags(cfg: &PathTagSettings, root: Option<&Path>, path: &Path) -> Vec<String> {
    let mut out = dir_tags(cfg, root, path);
    if let Some(stem) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
        for t in stem_tags(cfg, &stem) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

// ---------- 落库 ----------

/// 重挂资产 path 标签：删旧 origin='path' → 插新（同值 auto/manual 已存在则让位跳过）。
/// 返回 (inserted, removed)。仅动 origin='path' 行，auto/manual 永不触碰。
pub fn apply_tags(
    conn: &Connection,
    asset_id: i64,
    tags: &[String],
) -> rusqlite::Result<(usize, usize)> {
    let removed = conn.execute(
        "DELETE FROM tag WHERE assetId = ?1 AND origin = 'path'",
        rusqlite::params![asset_id],
    )? as usize;
    let mut inserted = 0usize;
    {
        let mut stmt = conn.prepare(
            "INSERT INTO tag (assetId, value, origin, score, createdAt)
             VALUES (?1, ?2, 'path', 1.0, unixepoch())
             ON CONFLICT(assetId, value) DO NOTHING",
        )?;
        for t in tags {
            inserted += stmt.execute(rusqlite::params![asset_id, t])? as usize;
        }
    }
    Ok((inserted, removed))
}

/// 按当前开关为某路径重解析并落库（导入/watcher/relink 用；source 路径优先于 absPath）。
/// 开关全关 → no-op（不回删已入库标签，裁决 2026-10-13）。
pub fn apply_for_path(
    conn: &Connection,
    asset_id: i64,
    src: &Path,
) -> rusqlite::Result<(usize, usize)> {
    let cfg = settings(conn);
    if cfg.disabled() {
        return Ok((0, 0));
    }
    let root = find_root(conn, src)?;
    let tags = derive_tags(&cfg, root.as_deref(), src);
    apply_tags(conn, asset_id, &tags)
}

/// 资产路径所属导入根：import_batches.rootPath 中为其前缀的最长者（无 → None）。
pub fn find_root(conn: &Connection, path: &Path) -> rusqlite::Result<Option<PathBuf>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT rootPath FROM import_batches
         WHERE status IN ('done','running','cancelled','failed')",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut best: Option<String> = None;
    for r in rows {
        let root = r?;
        if path.starts_with(Path::new(&root)) {
            if best.as_deref().map_or(true, |b| root.len() > b.len()) {
                best = Some(root);
            }
        }
    }
    Ok(best.map(PathBuf::from))
}

/// 按当前开关全量重扫（设置页「按当前开关重扫」按钮）：
/// 逐资产删旧 origin='path' 再按当前开关重插 —— 开关全关时等价于清空全部 path 标签
/// （重扫是显式重建动作，不受「开关关闭不回删」约束；该约束只限自动路径）。
/// copyin 资产 absPath 已是库内哈希目录：目录段无根可匹配，仅文件名词干生效。
pub fn rescan_all(conn: &Connection) -> rusqlite::Result<PathTagRescanResult> {
    let cfg = settings(conn);
    let assets: Vec<(i64, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT assetId, absPath, fileName FROM assets WHERE deleted = 0")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut res = PathTagRescanResult::default();
    let tx = conn.unchecked_transaction()?;
    for (aid, abs, file_name) in &assets {
        let abs_p = PathBuf::from(abs);
        let mut tags = dir_tags(&cfg, find_root(&tx, &abs_p)?.as_deref(), &abs_p);
        for t in stem_tags(&cfg, file_name) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
        let (ins, rem) = apply_tags(&tx, *aid, &tags)?;
        if ins > 0 || rem > 0 {
            res.assets += 1;
        }
        res.inserted += ins;
        res.removed += rem;
    }
    tx.commit()?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&c).unwrap();
        c
    }

    fn seed_asset(conn: &Connection, aid: i64, abs: &str, file: &str) {
        conn.execute(
            "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (?1, ?2, ?3, 'jpg', 1, 1, 1)",
            rusqlite::params![aid, abs, file],
        )
        .unwrap();
    }

    fn seed_batch(conn: &Connection, root: &str) {
        conn.execute(
            "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt, endedAt)
             VALUES (?1, 'reference', 'done', 1, 1, 1, 1)",
            rusqlite::params![root],
        )
        .unwrap();
    }

    fn tags_of(conn: &Connection, aid: i64, origin: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT value FROM tag WHERE assetId=?1 AND origin=?2 ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![aid, origin], |r| r.get::<_, String>(0))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    // ---------- 表驱动：噪声过滤与分隔符切词 ----------

    #[test]
    fn parse_name_table() {
        let cases: &[(&str, Vec<&str>)] = &[
            // 正常切词（空格/_/-/（）/·）
            ("旅行", vec!["旅行"]), // 中文无分隔符整段保留（规则③）
            ("海边 度假", vec!["海边", "度假"]),
            ("海边_度假", vec!["海边", "度假"]),
            ("my-dog", vec!["my", "dog"]),
            ("产品图（白底）", vec!["产品图", "白底"]), // 全角括号
            ("a·b", vec!["a", "b"]),
            ("ＭＹ ＤＯＧ", vec!["my", "dog"]), // 全角统一 + 小写归一
            ("Beach（２）", vec!["beach"]),     // 全角数字→纯数字段丢弃
            // 噪声丢弃（规则②）
            ("2024", vec![]),     // 纯数字段
            ("IMG_1234", vec![]), // 相机前缀+数字
            ("DSC0123", vec![]),
            ("VID0032", vec![]),
            ("Screenshot_2024-10-01", vec![]),
            ("微信图片_20240101123456", vec![]),
            ("微信图片20240101123456", vec![]), // 无分隔符整串也是噪声
            ("副本", vec![]),
            ("副本2", vec![]),
            ("photo (1)", vec!["photo"]),
            ("photo - copy", vec!["photo"]),
            // 长度上限（规则④：>6 字符丢弃，恰好 6 保留）
            ("六字标签刚好", vec!["六字标签刚好"]),
            ("七个字符的标签", vec![]),
            ("travel", vec!["travel"]),
            ("travels", vec![]),
            // 过滤后为空不产标签（规则⑤）/ 去重
            ("_", vec![]),
            ("", vec![]),
            ("猫 猫", vec!["猫"]),
            ("IMG", vec![]),
        ];
        for (raw, want) in cases {
            assert_eq!(&parse_name(raw), want, "parse_name({raw:?})");
        }
    }

    #[test]
    fn derive_dir_chain_and_root_excluded() {
        let both = PathTagSettings {
            use_dirs: true,
            use_filenames: true,
        };
        let root = Path::new("/data/pics");
        let p = root
            .join("旅行")
            .join("2024")
            .join("北京")
            .join("海边 度假.jpg");
        // ①父链每段保留（2024 纯数字丢弃）②文件名切词 ③根本身 pics 不入
        assert_eq!(
            derive_tags(&both, Some(root), &p),
            ["旅行", "北京", "海边", "度假"]
        );
        // 单开关
        let dirs_only = PathTagSettings {
            use_dirs: true,
            use_filenames: false,
        };
        assert_eq!(derive_tags(&dirs_only, Some(root), &p), ["旅行", "北京"]);
        let files_only = PathTagSettings {
            use_dirs: false,
            use_filenames: true,
        };
        assert_eq!(derive_tags(&files_only, Some(root), &p), ["海边", "度假"]);
        // 全关 / 无根（copyin 目的地路径）
        assert!(derive_tags(&PathTagSettings::default(), Some(root), &p).is_empty());
        assert_eq!(derive_tags(&files_only, None, &p), ["海边", "度假"]);
        // 噪声文件名只剩目录链
        let p2 = root.join("旅行").join("IMG_20240101_120001.jpg");
        assert_eq!(derive_tags(&both, Some(root), &p2), ["旅行"]);
        // 目录与文件名同名 token 合并去重
        let p3 = root.join("猫").join("猫.jpg");
        assert_eq!(derive_tags(&both, Some(root), &p3), ["猫"]);
    }

    // ---------- 落库：共存不合并 / 让位 / 重命名重解析 / 重扫 ----------

    #[test]
    fn apply_tags_coexist_and_yield_to_existing() {
        let c = conn();
        seed_asset(&c, 1, "/data/pics/旅行/a.jpg", "a.jpg");
        // auto 同名已存在 → path 让位跳过（不覆盖、不改 origin）
        c.execute(
            "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (1, '旅行', 'auto', 0.9, 1)",
            [],
        )
        .unwrap();
        let (ins, _) = apply_tags(&c, 1, &["旅行".into(), "海边".into()]).unwrap();
        assert_eq!(ins, 1, "同名 '旅行' 让位，只插入 '海边'");
        assert_eq!(tags_of(&c, 1, "auto"), ["旅行"]);
        assert_eq!(tags_of(&c, 1, "path"), ["海边"]);
    }

    #[test]
    fn apply_for_path_reparse_on_rename_keeps_manual() {
        let c = conn();
        seed_batch(&c, "/data/pics");
        seed_asset(&c, 1, "/data/pics/旅行/旧名.jpg", "旧名.jpg");
        c.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (1, '手动', 'manual', 1), (1, '狗', 'auto', 1)",
            [],
        )
        .unwrap();
        core_storage::settings::set_json(
            &c,
            PATH_TAG_SETTINGS_KEY,
            &PathTagSettings {
                use_dirs: true,
                use_filenames: true,
            },
        )
        .unwrap();

        // 初次解析
        apply_for_path(&c, 1, Path::new("/data/pics/旅行/旧名.jpg")).unwrap();
        assert_eq!(tags_of(&c, 1, "path"), ["旅行", "旧名"]);

        // 重命名 → 仅重解析 origin='path'：manual/auto 不动
        apply_for_path(&c, 1, Path::new("/data/pics/旅行/新名.jpg")).unwrap();
        assert_eq!(tags_of(&c, 1, "path"), ["旅行", "新名"]);
        assert_eq!(tags_of(&c, 1, "manual"), ["手动"]);
        assert_eq!(tags_of(&c, 1, "auto"), ["狗"]);

        // 开关全关 → no-op（不回删）
        core_storage::settings::set_json(&c, PATH_TAG_SETTINGS_KEY, &PathTagSettings::default())
            .unwrap();
        apply_for_path(&c, 1, Path::new("/data/pics/别的/xx.jpg")).unwrap();
        assert_eq!(tags_of(&c, 1, "path"), ["旅行", "新名"], "关态不回删");
    }

    #[test]
    fn find_root_picks_longest_prefix() {
        let c = conn();
        seed_batch(&c, "/data");
        seed_batch(&c, "/data/pics");
        assert_eq!(
            find_root(&c, Path::new("/data/pics/a/b.jpg")).unwrap(),
            Some(PathBuf::from("/data/pics"))
        );
        assert_eq!(find_root(&c, Path::new("/elsewhere/x.jpg")).unwrap(), None);
    }

    #[test]
    fn rescan_rebuilds_per_current_switches() {
        let c = conn();
        seed_batch(&c, "/data/pics");
        seed_asset(&c, 1, "/data/pics/旅行/海边.jpg", "海边.jpg");
        seed_asset(&c, 2, "/data/pics/副本.jpg", "副本.jpg");
        // 先全关导入（无标签），开启后重扫补齐（PLAN 验收）
        let r0 = rescan_all(&c).unwrap();
        assert_eq!((r0.inserted, r0.removed), (0, 0));

        core_storage::settings::set_json(
            &c,
            PATH_TAG_SETTINGS_KEY,
            &PathTagSettings {
                use_dirs: true,
                use_filenames: true,
            },
        )
        .unwrap();
        let r1 = rescan_all(&c).unwrap();
        assert_eq!(r1.inserted, 2, "asset1: 旅行+海边；asset2: 副本=噪声 0");
        assert_eq!(tags_of(&c, 1, "path"), ["旅行", "海边"]);
        assert!(tags_of(&c, 2, "path").is_empty());

        // 关掉目录开关重扫：目录段标签被移除、文件名词保留（显式重建语义）
        core_storage::settings::set_json(
            &c,
            PATH_TAG_SETTINGS_KEY,
            &PathTagSettings {
                use_dirs: false,
                use_filenames: true,
            },
        )
        .unwrap();
        let _ = rescan_all(&c).unwrap();
        assert_eq!(tags_of(&c, 1, "path"), ["海边"]);

        // 全关重扫 = 清空全部 path 标签（显式动作）
        core_storage::settings::set_json(&c, PATH_TAG_SETTINGS_KEY, &PathTagSettings::default())
            .unwrap();
        let r2 = rescan_all(&c).unwrap();
        assert_eq!(r2.removed, 1);
        assert!(tags_of(&c, 1, "path").is_empty());
    }
}
