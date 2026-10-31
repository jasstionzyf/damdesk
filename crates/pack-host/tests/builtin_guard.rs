//! N11 架构守卫：**内置核心功能不得被包成插件**。
//!
//! 口径锚点：damdesk-product `references/plugin-design-and-ui.md` §2 三档分层（第一档「内置核心
//! 功能 = 批量打标 / 查重去重 / EXIF·OCR / 向量索引与语义检索，不装不卸」）+ 判据第 3 条
//! 「已实现在宿主里的能力不因为『能包成插件』而重包」+ §8 硬边界。
//! 模型 / provider 可换 —— 那是**配置**，不是插件；本守卫不碰配置面。
//!
//! 为什么必须钉死：内置功能一旦被重包成插件，用户就得「先装插件才能打标」，卸载插件还会让既有
//! 结论行与索引语义漂移（§8 第 4 条）。这是不可逆的产品口径，得用可执行断言钉住，不能靠记性。
//! 所以下面五个面逐个扫，命中时打印 **规则 + 文件:行号 + 命中的词 + 为什么错**，不是 `assert!(true)`。
//!
//! | 规则 | 面 | 判据 |
//! |---|---|---|
//! | R1 | 插件 manifest（`packs/plugins/*/manifest.json`） | 不得引用内置引擎 crate；不得用内置族名做 `id`；`capabilities` 必须是宿主已知能力（不许自造 `tag.batch` / `dedup.phash` 这类族名能力） |
//! | R2 | 包里会行进 `.dampack` 的代码 / 资源 | 不得引用内置引擎 crate —— 包不许自己实现打标/查重/EXIF/OCR/向量/检索 |
//! | R3 | `.dampack` 打包清单（真打一次归档再读回） | 条目名与条目内容同 R2 —— 打包这一步不许把内置实现带进包 |
//! | R4 | `pack-host` 依赖闭包 | `engine-tag`/`engine-dedup`/`engine-exif`/`engine-embed`/`engine-search` 不得出现（出现 = 宿主内核开始「替包实现」内置功能）；`engine-ocr` 是**宿主侧能力网关 provider**，必须在闭包里 —— 见 [`OCR_GATEWAY_PROVIDER`] |
//! | R5 | 插件注册表与列表源码（`app-web::compliance_api` 的 `plugin_packs_view` / `pack-host::registry`）+ 插件目录名 | 不得按内置族把内置功能注册成插件；`/api/compliance/packs.plugins` 的 id 空间里不得出现内置族 |
//!
//! 正向断言（防「删干净就全绿」）：
//! * P1 六个内置引擎 crate 必须都在工作区（内置 ≠ 删掉）；
//! * P2 五个「包侧禁入」引擎必须被 pack-host 之外的宿主侧 crate 真实引用（证明它们住在宿主里）；
//! * R4 反证 OCR 的宿主实现必须在 pack-host 依赖闭包内（OCR 由宿主做、包只调网关）。
//!
//! 扫描面本身也不许空（包目录 / 注册表源码找不到直接判违规），否则守卫会退化成永远 PASS。
//!
//! 跑法：`cargo test -p pack-host --test builtin_guard -- --nocapture`
//!   或 `bash scripts/builtin_guard.sh`

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// 六类内置能力 → 宿主里的唯一实现 crate。
const BUILTIN_ENGINES: [(&str, &str); 6] = [
    ("engine-tag", "批量打标"),
    ("engine-dedup", "查重去重"),
    ("engine-exif", "EXIF"),
    ("engine-ocr", "OCR"),
    ("engine-embed", "向量索引（embedding）"),
    ("engine-search", "语义检索"),
];

/// 包侧（manifest / 包代码 / 归档 / 注册表）一律禁入的内置引擎：这五个是「整条用户功能」的实现，
/// 只能住在宿主的 L1 + 用户侧服务里。
const PACK_FORBIDDEN_ENGINES: [&str; 5] = [
    "engine-tag",
    "engine-dedup",
    "engine-exif",
    "engine-embed",
    "engine-search",
];

/// `pack-host` 依赖闭包里**允许且必须**存在的那一个内置引擎。
///
/// 为什么它是例外而不是漏网：能力网关 `POST /detect/ocr`（协议 §6，能力名 `detect.ocr`）的实现
/// 就是 `engine-ocr`，包调用网关 = 用宿主实现。把它从 pack-host 摘掉不是加固守，是把网关端点
/// 拆了（口径出处：`crates/pack-host/Cargo.toml` 依赖段注释）。真正的红线「包不许自己实现 OCR」
/// 由 R1~R3 保证 —— 那三条对 `engine-ocr` 一视同仁。
const OCR_GATEWAY_PROVIDER: &str = "engine-ocr";

/// 内置族名。出现在插件 `id` / 插件目录名 / 自造能力名里 = 试图把内置功能包成插件。
const BUILTIN_FAMILIES: [&str; 6] = ["tag", "dedup", "exif", "ocr", "embed", "search"];

// ---------------------------------------------------------------------------
// 报告器
// ---------------------------------------------------------------------------

struct Guard {
    root: PathBuf,
    hits: Vec<String>,
    scan_lines: Vec<String>,
    notes: Vec<String>,
}

impl Guard {
    fn new() -> Self {
        // tests/ 的 CARGO_MANIFEST_DIR = 仓库/crates/pack-host → 上两级 = 仓库根
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("仓库根解析失败")
            .to_path_buf();
        Self {
            root,
            hits: Vec::new(),
            scan_lines: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    /// 记一条违规：规则 + 文件:行号 + 命中的词 + 为什么错。
    fn hit(&mut self, rule: &str, path: &Path, line: usize, needle: &str, why: &str) {
        let loc = if line == 0 {
            self.rel(path)
        } else {
            format!("{}:{}", self.rel(path), line)
        };
        self.hit_at(rule, &loc, needle, why);
    }

    fn hit_at(&mut self, rule: &str, loc: &str, needle: &str, why: &str) {
        self.hits
            .push(format!("{rule}  {loc}  命中「{needle}」 —— {why}"));
    }
}

// ---------------------------------------------------------------------------
// 词表
// ---------------------------------------------------------------------------

/// 包侧扫描词表：(要匹配的字面量, 为什么它违规)。
fn forbidden_needles() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = BUILTIN_ENGINES
        .iter()
        .map(|(crate_name, label)| {
            (
                (*crate_name).to_string(),
                format!("内置功能「{label}」的宿主实现 crate，不得出现在包侧"),
            )
        })
        .collect();
    for fam in BUILTIN_FAMILIES {
        out.push((
            format!("com.soujpg.pack.{fam}"),
            format!("用内置族名「{fam}」做插件 id = 把内置功能重包成插件"),
        ));
    }
    out
}

/// 只有 crate 名（R4 / R5 用）。
fn engine_needles() -> Vec<(String, String)> {
    BUILTIN_ENGINES
        .iter()
        .map(|(crate_name, label)| {
            (
                (*crate_name).to_string(),
                format!("内置功能「{label}」的宿主实现 crate"),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 文件扫描
// ---------------------------------------------------------------------------

/// 逐行扫一个文件，命中即记违规。返回 (是否文本文件, 扫过的行数)。
fn scan_file(g: &mut Guard, rule: &str, path: &Path, needles: &[(String, String)]) -> (bool, usize) {
    let Ok(bytes) = fs::read(path) else {
        return (false, 0);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return (false, 0); // 二进制资源（图片/字体）不参与词面扫描
    };
    let mut lines = 0usize;
    for (i, raw) in text.lines().enumerate() {
        lines += 1;
        for (needle, why) in needles {
            if raw.contains(needle.as_str()) {
                g.hit(rule, path, i + 1, needle, why);
            }
        }
    }
    (true, lines)
}

/// 递归列出目录下所有文件（跳过 `.git` / `__pycache__` / venv / node_modules）。
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if p.is_dir() {
            if matches!(
                name,
                ".git" | "__pycache__" | ".venv" | "venv" | "node_modules"
            ) {
                continue;
            }
            walk_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Cargo.toml 依赖解析（带行号 —— 违规要能点到行）
// ---------------------------------------------------------------------------

struct CrateToml {
    name: String,
    path: PathBuf,
    /// (被依赖的候选标识, 行号, 原始行)
    refs: Vec<(String, usize, String)>,
}

fn parse_crate_toml(dir_name: &str, path: &Path) -> Option<CrateToml> {
    let text = fs::read_to_string(path).ok()?;
    let mut name = dir_name.to_string();
    let mut refs: Vec<(String, usize, String)> = Vec::new();
    let mut section = String::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_ascii_lowercase();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let is_dep_zone = matches!(
            section.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) || (section.starts_with("target.") && section.ends_with("dependencies"));
        if is_dep_zone {
            if let Some((k, _)) = line.split_once('=') {
                let raw_key = k.trim().trim_matches('"');
                // TOML 里同一个依赖有三种写法：`dep = "1"` / `dep = { path = ... }` /
                // `dep.workspace = true`（点号键）。后者若不剥前缀就会被整条丢掉，
                // 依赖闭包会少一半（engine-classic / core-* 全丢），守卫会假 PASS。
                let key = raw_key.split('.').next().unwrap_or(raw_key);
                if !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    refs.push((key.to_string(), i + 1, raw.to_string()));
                }
            }
        } else if section == "package" {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "name" {
                    name = v.trim().trim_matches('"').to_string();
                }
            }
        } else if section == "features" {
            // 形如 `local-infer = ["engine-ocr/local-infer", "dep:engine-tag"]`
            for tok in line.split(|c: char| {
                c == '"' || c == ',' || c == '[' || c == ']' || c == '=' || c == ' '
            }) {
                let tok = tok.trim();
                if tok.is_empty() || (!tok.contains('/') && !tok.starts_with("dep:")) {
                    continue;
                }
                let t = tok.strip_prefix("dep:").unwrap_or(tok);
                let crate_name = t.split('/').next().unwrap_or(t);
                refs.push((crate_name.to_string(), i + 1, raw.to_string()));
            }
        }
    }
    Some(CrateToml {
        name,
        path: path.to_path_buf(),
        refs,
    })
}

fn workspace_crates(root: &Path) -> Vec<CrateToml> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root.join("crates")) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let toml = dir.join("Cargo.toml");
        if !toml.is_file() {
            continue;
        }
        let dir_name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if let Some(c) = parse_crate_toml(dir_name, &toml) {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// R1：插件 manifest
// ---------------------------------------------------------------------------

fn plugin_pack_dirs(root: &Path) -> Vec<PathBuf> {
    let base = root.join("packs").join("plugins");
    let Ok(rd) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    out
}

fn scan_manifests(g: &mut Guard) -> usize {
    let needles = forbidden_needles();
    let dirs = plugin_pack_dirs(&g.root);
    if dirs.is_empty() {
        g.hit_at(
            "R1",
            "packs/plugins",
            "(目录缺失)",
            "插件包目录不存在 → 扫描面为空，守卫会假 PASS",
        );
        return 0;
    }
    let mut scanned = 0usize;
    for dir in &dirs {
        let manifest = dir.join("manifest.json");
        if !manifest.is_file() {
            let rel = g.rel(dir);
            g.hit_at("R1", &rel, "(缺 manifest.json)", "插件包目录里没有 manifest，不是可装载的包");
            continue;
        }
        scan_file(g, "R1", &manifest, &needles);
        scanned += 1;

        let Ok(raw) = fs::read_to_string(&manifest) else {
            let rel = g.rel(&manifest);
            g.hit_at("R1", &rel, "(manifest 不可读)", "manifest 读不出来 → 扫描面为空");
            continue;
        };
        let line_of = |needle: &str| {
            raw.lines()
                .position(|l| l.contains(needle))
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        // 用 serde_json 读原文，而不是 `Manifest::from_path`：拒装口径里还夹着 `api_version`
        // 严格相等这类**与架构无关**的校验，混进来会让守卫因为别的原因红（红得没有信息量）。
        // 这里只吃 `id` / `capabilities` 两栏；拒装层面的问题记 note，归 manifest 校验单测管。
        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                let rel = g.rel(&manifest);
                g.hit_at("R1", &rel, "(manifest 非法 JSON)", &format!("{e}"));
                continue;
            }
        };
        let caps: Vec<String> = value
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // 能力白名单 + 内置族能力名（自造 tag.batch / dedup.phash 这类）
        for cap in &caps {
            if !pack_host::manifest::KNOWN_CAPABILITIES.contains(&cap.as_str()) {
                let why = match BUILTIN_FAMILIES
                    .iter()
                    .find(|f| cap.split('.').next() == Some(**f))
                {
                    Some(fam) => format!(
                        "自造能力名「{cap}」落在内置族「{fam}」上 —— 内置功能只能由宿主实现，不能变成插件能力"
                    ),
                    None => format!(
                        "能力「{cap}」不在宿主能力白名单 {:?} 里",
                        pack_host::manifest::KNOWN_CAPABILITIES
                    ),
                };
                let line = line_of(cap.as_str());
                g.hit("R1", &manifest, line, cap, &why);
            }
        }
        // 插件 id：内置族不得成为插件身份
        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
            for fam in BUILTIN_FAMILIES {
                let bad = format!("com.soujpg.pack.{fam}");
                if id == bad || id.ends_with(&format!(".{fam}")) {
                    let line = line_of(id);
                    g.hit(
                        "R1",
                        &manifest,
                        line,
                        id,
                        "插件 id 落在内置族命名空间里 = 把内置功能注册成插件",
                    );
                }
            }
        }
        // 拒装面（api_version / 未知键 / 能力白名单）只记 note：那是 manifest 校验单测的判据
        if let Err(e) = pack_host::manifest::Manifest::from_path(&manifest) {
            let rel = g.rel(&manifest);
            g.notes
                .push(format!("R1 {rel} 按宿主拒装口径不可装载（非本守卫判据，仅记录）：{e}"));
        }
    }
    scanned
}

// ---------------------------------------------------------------------------
// R2：包里会行进 `.dampack` 的代码 / 资源
// ---------------------------------------------------------------------------

fn scan_pack_payload(g: &mut Guard) -> (usize, usize) {
    let needles = forbidden_needles();
    let mut files = 0usize;
    let mut lines = 0usize;
    for dir in plugin_pack_dirs(&g.root) {
        let mut all = Vec::new();
        walk_files(&dir, &mut all);
        for f in all {
            if f.file_name().and_then(|s| s.to_str()) == Some("manifest.json") {
                continue; // R1 已扫
            }
            let (text, n) = scan_file(g, "R2", &f, &needles);
            if text {
                files += 1;
                lines += n;
            }
        }
    }
    (files, lines)
}

// ---------------------------------------------------------------------------
// R3：`.dampack` 打包清单（真打一次再读回）
// ---------------------------------------------------------------------------

fn scan_dampack(g: &mut Guard) -> (usize, usize) {
    let needles = forbidden_needles();
    let mut archives = 0usize;
    let mut entries_total = 0usize;
    for dir in plugin_pack_dirs(&g.root) {
        let mut files = Vec::new();
        walk_files(&dir, &mut files);
        let mut full: Vec<(String, Vec<u8>)> = vec![(
            "manifest.json".to_string(),
            fs::read(dir.join("manifest.json")).unwrap_or_default(),
        )];
        for f in files {
            let Ok(rel) = f.strip_prefix(&dir) else {
                continue;
            };
            let name = rel.to_string_lossy().replace('\\', "/");
            if name == "manifest.json" {
                continue;
            }
            if let Ok(body) = fs::read(&f) {
                full.push((name, body));
            }
        }

        // 真落一次 `.dampack`（zip 归档）再读回来：打包这一步不许把内置实现带进包
        let mut buf = Cursor::new(Vec::new());
        if let Err(e) = pack_host::write_archive(&mut buf, &full) {
            let rel = g.rel(&dir);
            g.hit_at("R3", &rel, "(写归档失败)", &format!("{e}"));
            continue;
        }
        let bytes = buf.into_inner();
        let mut zip = match zip::ZipArchive::new(Cursor::new(bytes)) {
            Ok(z) => z,
            Err(e) => {
                let rel = g.rel(&dir);
                g.hit_at("R3", &rel, "(读归档失败)", &format!("{e}"));
                continue;
            }
        };
        archives += 1;
        let mut names: Vec<String> = Vec::new();
        for i in 0..zip.len() {
            let mut entry = match zip.by_index(i) {
                Ok(e) => e,
                Err(e) => {
                    let rel = g.rel(&dir);
                    g.hit_at("R3", &rel, "(归档条目读失败)", &format!("{e}"));
                    continue;
                }
            };
            let name = match entry.name() {
                Ok(n) => n.to_string(),
                Err(e) => {
                    let rel = g.rel(&dir);
                    g.hit_at("R3", &rel, "(归档条目名读失败)", &format!("{e}"));
                    continue;
                }
            };
            names.push(name.clone());
            entries_total += 1;
            for (needle, why) in &needles {
                if name.contains(needle.as_str()) {
                    let loc = format!("{} → 归档条目「{name}」", g.rel(&dir));
                    g.hit_at("R3", &loc, needle, why);
                }
            }
            let mut body = String::new();
            if entry.read_to_string(&mut body).is_ok() {
                for (n, raw) in body.lines().enumerate() {
                    for (needle, why) in &needles {
                        if raw.contains(needle.as_str()) {
                            let loc = format!("{} → 归档条目「{name}」:{}", g.rel(&dir), n + 1);
                            g.hit_at("R3", &loc, needle, why);
                        }
                    }
                }
            }
        }
        let rel = g.rel(&dir);
        if !names.iter().any(|n| n == "manifest.json") {
            g.hit_at("R3", &rel, "(归档缺 manifest.json)", "打包清单里没有 manifest，装不了");
        }
        if names.len() < 2 {
            g.hit_at(
                "R3",
                &rel,
                "(归档条目 < 2)",
                "包只有 manifest 没有实现文件 —— 扫描面为空",
            );
        }
    }
    (archives, entries_total)
}

// ---------------------------------------------------------------------------
// R4：pack-host 依赖闭包
// ---------------------------------------------------------------------------

fn scan_pack_host_closure(g: &mut Guard) -> usize {
    let crates = workspace_crates(&g.root);
    if crates.is_empty() {
        g.hit_at(
            "R4",
            "crates/",
            "(工作区 crate 为空)",
            "依赖图扫描面为空，守卫会假 PASS",
        );
        return 0;
    }
    let names: BTreeSet<String> = crates.iter().map(|c| c.name.clone()).collect();
    let mut edges: BTreeMap<String, Vec<(String, PathBuf, usize)>> = BTreeMap::new();
    for c in &crates {
        let outs = edges.entry(c.name.clone()).or_default();
        for (dep, line, _raw) in &c.refs {
            if names.contains(dep) && dep != &c.name {
                outs.push((dep.clone(), c.path.clone(), *line));
            }
        }
    }

    // BFS：从 pack-host 出发，记住每个 crate 是被哪条边拉进闭包的（报违规要指到边）
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut introduced: BTreeMap<String, (PathBuf, usize)> = BTreeMap::new();
    let mut q: VecDeque<String> = VecDeque::new();
    seen.insert("pack-host".to_string());
    q.push_back("pack-host".to_string());
    while let Some(cur) = q.pop_front() {
        for (dep, file, line) in edges.get(&cur).cloned().unwrap_or_default() {
            if seen.insert(dep.clone()) {
                introduced.insert(dep.clone(), (file, line));
                q.push_back(dep);
            }
        }
    }

    g.notes.push(format!(
        "R4 pack-host 依赖闭包（{} 个）：{}",
        seen.len(),
        seen.iter().cloned().collect::<Vec<_>>().join(", ")
    ));

    for forbidden in PACK_FORBIDDEN_ENGINES {
        if let Some((file, line)) = introduced.get(forbidden).cloned() {
            g.hit(
                "R4",
                &file,
                line,
                forbidden,
                "内置功能实现被拉进 pack-host 依赖闭包 —— 宿主内核开始替包实现内置功能（这些只能留在宿主用户侧）",
            );
        }
    }
    if seen.contains(OCR_GATEWAY_PROVIDER) {
        let at = introduced
            .get(OCR_GATEWAY_PROVIDER)
            .map(|(f, l)| format!("{}:{}", g.rel(f), l))
            .unwrap_or_else(|| "-".into());
        g.notes.push(format!(
            "R4 {OCR_GATEWAY_PROVIDER} 在闭包内 → 允许（宿主侧 /detect/ocr provider，引入位置 {at}）"
        ));
    } else {
        g.hit_at(
            "R4",
            "crates/pack-host/Cargo.toml",
            OCR_GATEWAY_PROVIDER,
            "反证失败：OCR 必须由宿主实现并挂在 pack-host 能力网关上（/detect/ocr）",
        );
    }
    for (dep, file, line) in edges.get("pack-host").cloned().unwrap_or_default() {
        if BUILTIN_ENGINES.iter().any(|(e, _)| *e == dep) {
            let loc = format!("{}:{}", g.rel(&file), line);
            g.notes.push(format!("R4 pack-host 直接依赖 {dep}（{loc}）"));
        }
    }
    seen.len()
}

// ---------------------------------------------------------------------------
// R5：插件注册表 / 列表源码 / 插件目录名
// ---------------------------------------------------------------------------

/// 取一个顶层函数的源码区间（`fn <name>(` 行 → 下一个列首 `}` 行，rustfmt 下的稳定形状）。
fn function_span(lines: &[&str], name: &str) -> Option<(usize, usize)> {
    let pat = format!("fn {name}(");
    let start = lines.iter().position(|l| {
        let t = l.trim_start();
        (t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("pub async fn ")
            || t.starts_with("async fn ")
            || t.starts_with("pub(crate) fn "))
            && t.contains(&pat)
    })?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim_end() == "}")
        .map(|(i, _)| i)?;
    Some((start, end))
}

fn scan_registry_surfaces(g: &mut Guard) -> usize {
    let needles = engine_needles();
    let mut scanned = 0usize;

    // (1) 插件库安装层整份文件：它的全部输出都会成为插件列表的行
    for rel in ["crates/pack-host/src/registry.rs"] {
        let f = g.root.join(rel);
        if !f.is_file() {
            g.hit_at("R5", rel, "(文件缺失)", "插件注册表源码找不到 → 扫描面为空");
            continue;
        }
        scan_file(g, "R5", &f, &needles);
        scanned += 1;
    }

    // (2) 列表/注册函数的函数体：只看「插件列表怎么算出来的」这段，不做整文件扫描 ——
    //     `compliance_api.rs` 里本来就合法地引用内置引擎（比如 VL 通道复用打标的
    //     `engine-tag::vl_image` 注释），整文件扫会把合法引用误判成「包成插件」。
    let listing = g.root.join("crates/app-web/src/compliance_api.rs");
    let Ok(text) = fs::read_to_string(&listing) else {
        g.hit_at(
            "R5",
            "crates/app-web/src/compliance_api.rs",
            "(文件缺失)",
            "插件列表源码找不到 → 扫描面为空",
        );
        return scanned;
    };
    let lines: Vec<&str> = text.lines().collect();
    // 防空扫：插件列表的唯一入口必须还在
    for marker in ["fn plugin_packs_view", "\"plugins\": plugin_packs_view"] {
        if !text.contains(marker) {
            g.hit_at(
                "R5",
                "crates/app-web/src/compliance_api.rs",
                marker,
                "插件列表入口不见了 —— R5 的扫描面对空了",
            );
        }
    }
    // 扫描面 = 插件注册/列表链路上的函数。`builtin_plugin`（内置 YAML 行业包的注册函数）
    // 已随 IP-K16 插件唯一化删除，不再列在这里；函数名一旦全部失效，上面的标记检查会先报红。
    let registry_fns = [
        "plugin_packs_view",
        "plugin_pack_detail",
        "pack_list",
        "plugin_state",
        "plugin_state_for",
        "register_pack",
        "upsert_pack_row",
        "pack_row_json",
        "plugin_identity_id",
    ];
    for name in registry_fns {
        let Some((start, end)) = function_span(&lines, name) else {
            g.hit_at(
                "R5",
                "crates/app-web/src/compliance_api.rs",
                &format!("fn {name}"),
                "插件注册/列表函数不见了 —— 扫描面对空，守卫会假 PASS",
            );
            continue;
        };
        for (i, raw) in lines.iter().enumerate().take(end + 1).skip(start) {
            for (needle, why) in &needles {
                if raw.contains(needle.as_str()) {
                    g.hit(
                        "R5",
                        &listing,
                        i + 1,
                        needle,
                        &format!("{why} —— 出现在插件注册/列表函数 `{name}` 里"),
                    );
                }
            }
        }
        scanned += 1;
    }

    // (3) 内置插件白名单（`BUILTIN_PLUGIN_IDS`）不得把内置族当插件 id
    for (i, raw) in lines.iter().enumerate() {
        if raw.contains("BUILTIN_PLUGIN_IDS") && raw.contains('=') {
            for (needle, why) in &needles {
                if raw.contains(needle.as_str()) {
                    g.hit("R5", &listing, i + 1, needle, why);
                }
            }
            for fam in BUILTIN_FAMILIES {
                if raw.contains(&format!("\"{fam}\"")) || raw.contains(&format!("pack.{fam}")) {
                    g.hit(
                        "R5",
                        &listing,
                        i + 1,
                        fam,
                        "内置族名进了插件白名单 = 内置功能被注册成插件",
                    );
                }
            }
        }
    }

    // (4) 插件目录名不得以内置族命名
    for fam in BUILTIN_FAMILIES {
        let d = g.root.join("packs").join("plugins").join(fam);
        if d.exists() {
            let rel = g.rel(&d);
            g.hit_at("R5", &rel, fam, "内置族名出现在插件目录里 = 内置功能被包成插件");
        }
    }
    scanned
}

// ---------------------------------------------------------------------------
// P1 / P2：正向断言（内置能力必须住在宿主里，不能靠删干净过关）
// ---------------------------------------------------------------------------

fn scan_host_wiring(g: &mut Guard) {
    let crates = workspace_crates(&g.root);
    let names: BTreeSet<String> = crates.iter().map(|c| c.name.clone()).collect();
    for (engine, label) in BUILTIN_ENGINES {
        if !names.contains(engine) {
            let loc = format!("crates/{engine}");
            g.hit_at(
                "P1",
                &loc,
                engine,
                &format!(
                    "内置功能「{label}」的引擎 crate 不在工作区 —— 内置能力被删了，不是「不包插件」"
                ),
            );
        }
    }
    for engine in PACK_FORBIDDEN_ENGINES {
        let users: Vec<String> = crates
            .iter()
            .filter(|c| c.name != "pack-host" && c.refs.iter().any(|(d, _, _)| d == engine))
            .map(|c| c.name.clone())
            .collect();
        // dev-e2e 是测试夹具，不算「宿主侧真实使用」
        let real: Vec<&String> = users.iter().filter(|u| u.as_str() != "dev-e2e").collect();
        if real.is_empty() {
            let loc = format!("crates/{engine}");
            g.hit_at(
                "P2",
                &loc,
                engine,
                "该内置引擎没有被任何宿主侧 crate 真实引用 —— 既没包成插件也没住在宿主里，内置能力名存实亡",
            );
        } else {
            g.notes.push(format!(
                "P2 {engine} 宿主侧引用：{}",
                real.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[test]
fn builtin_features_must_not_be_packaged_as_plugins() {
    let mut g = Guard::new();

    let manifest_count = scan_manifests(&mut g);
    g.scan_lines
        .push(format!("R1 插件 manifest        {manifest_count} 个文件"));
    let (payload_files, payload_lines) = scan_pack_payload(&mut g);
    g.scan_lines.push(format!(
        "R2 包内代码/资源        {payload_files} 个文本文件 / {payload_lines} 行"
    ));
    let (archives, entries) = scan_dampack(&mut g);
    g.scan_lines.push(format!(
        "R3 .dampack 打包清单    {archives} 个归档 / {entries} 条目"
    ));
    let closure = scan_pack_host_closure(&mut g);
    g.scan_lines
        .push(format!("R4 pack-host 依赖闭包   {closure} 个 crate"));
    let reg_files = scan_registry_surfaces(&mut g);
    g.scan_lines.push(format!(
        "R5 注册表/列表源码      {reg_files} 个文件 + {} 个内置族目录名检查",
        BUILTIN_FAMILIES.len()
    ));
    scan_host_wiring(&mut g);

    println!("\n[N11] 内置功能架构守卫 @ {}", g.root.display());
    println!("[N11] 扫描面：");
    for l in &g.scan_lines {
        println!("  {l}");
    }
    println!("[N11] 口径记录：");
    for l in &g.notes {
        println!("  {l}");
    }
    if g.hits.is_empty() {
        println!(
            "[N11] 违规 0 项 → PASS（打标/查重/EXIF/OCR/向量/检索仍在宿主里，未出现在插件 manifest / 包代码 / .dampack / pack-host 依赖闭包 / 插件列表）\n"
        );
    } else {
        println!("[N11] 违规 {} 项 → FAIL：", g.hits.len());
        for h in &g.hits {
            println!("  {h}");
        }
        println!();
        panic!(
            "内置功能被包成插件：{} 项违规（见上方 规则/文件:行号/命中的词）",
            g.hits.len()
        );
    }
}
