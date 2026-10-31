//! C8「装载前 lint」命令行：对任意 pack 目录做装载门禁同源体检，
//! **不落库、不激活** —— 给作者/CI 用。
//!
//! 用法：
//!   pack_lint <pack 目录> [...]      # 退出码 0 = 全干净；1 = 有 issue；2 = 用法错
//!
//! 两种 pack 形态，按目录自动分派（与各自的引擎装载路径同一套判定）：
//! - **插件包**（v1.1 协议，`manifest.json` 在盘）：装载门禁 = `pack_host::load_pack`
//!   （manifest 键级 schema + 资源声明在盘），app-web 导入/激活走的就是它；
//! - **合规行业包**（四文件 `taxonomy/rules/pipeline/report.yaml`）：
//!   键级（未知键 = 引擎静默吞掉的键）由 `core_config::lint` 在 `Pack::from_dir` 内**硬拒**；
//!   注册表层（未知检测器 / 未知 params 键 / 未知插件）由 `svc_pipeline::compliance_lint`
//!   在导入/激活门禁**硬拒**；本命令把两层一次列清，等价于「装载前先跑一遍门禁」。
//!
//! 输出：逐 issue 一行（`文件: 键路径: [code] 说明（已知: ...）`）+ 已知取值清单（注册表全量）。

use std::path::PathBuf;
use std::process::ExitCode;

/// 单个 pack 的 issue 清单（分派后统一成字符串行，插件/合规两种形态输出同形）。
fn lint_pack_dir(path: &std::path::Path) -> Vec<String> {
    if path.join("manifest.json").is_file() {
        // 插件包：与 app-web 导入门禁同一入口（manifest schema + 声明资源在盘）
        return match pack_host::load_pack(path) {
            Ok(_) => Vec::new(),
            Err(e) => vec![format!("manifest.json: [load_failed] {e}")],
        };
    }
    svc_pipeline::compliance_lint::lint_dir(path)
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn main() -> ExitCode {
    let dirs: Vec<String> = std::env::args().skip(1).collect();
    if dirs.is_empty() {
        eprintln!("用法: pack_lint <pack 目录> [<pack 目录> ...]");
        return ExitCode::from(2);
    }
    let mut bad = 0usize;
    for d in &dirs {
        let path = PathBuf::from(d);
        let issues = lint_pack_dir(&path);
        if issues.is_empty() {
            println!(
                "OK    {} — 装载前 lint 干净（与装载门禁同源）",
                path.display()
            );
            continue;
        }
        bad += 1;
        println!(
            "FAIL  {} — 装载前 lint 未通过（{} 条）：",
            path.display(),
            issues.len()
        );
        for i in &issues {
            println!("        {i}");
        }
    }
    // 已知取值清单：作者对账用（注册表全量，键级白名单见 issue 内的「已知:」）
    println!("--- 已知取值 ---");
    println!("classic 检测器: {}", engine_classic::registry::known_ids());
    println!("vl 检测器: {}", svc_pipeline::compliance_vl::vl_known_ids());
    println!(
        "插件（fixer.type / do 步骤 plugin）: {}",
        svc_pipeline::compliance_plugins::PLUGINS
            .iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
            .join(", ")
    );
    if bad > 0 {
        println!("RESULT: {bad}/{} 个 pack 未通过", dirs.len());
        return ExitCode::FAILURE;
    }
    println!("RESULT: {}/{} 个 pack 全部通过", dirs.len(), dirs.len());
    ExitCode::SUCCESS
}
