//! C8「装载前 lint」命令行：对任意 pack 目录做完整体检（键级 schema + 注册表层），
//! **不产出 Pack、不落库、不激活** —— 给作者/CI 用。
//!
//! 用法：
//!   pack_lint <pack 目录> [...]      # 退出码 0 = 全干净；1 = 有 issue；2 = 用法错
//!
//! 与引擎装载的关系（同一套判定，两道口）：
//! - 键级（未知键 = 引擎静默吞掉的键）由 `core_config::lint` 在 `Pack::from_dir` 内**硬拒**；
//! - 注册表层（未知检测器 / 未知 params 键 / 未知插件）由 `svc_pipeline::compliance_lint` 在
//!   app-web 的导入/激活门禁**硬拒**；本命令把两层一次列清，等价于「装载前先跑一遍门禁」。
//!
//! 输出：逐 issue 一行（`文件: 键路径: [code] 说明（已知: ...）`）+ 已知取值清单（注册表全量）。

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let dirs: Vec<String> = std::env::args().skip(1).collect();
    if dirs.is_empty() {
        eprintln!("用法: pack_lint <pack 目录> [<pack 目录> ...]");
        return ExitCode::from(2);
    }
    let mut bad = 0usize;
    for d in &dirs {
        let path = PathBuf::from(d);
        let issues = svc_pipeline::compliance_lint::lint_dir(&path);
        if issues.is_empty() {
            println!("OK    {} — 装载前 lint 干净（键级 + 注册表层）", path.display());
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
    println!(
        "classic 检测器: {}",
        engine_classic::registry::known_ids()
    );
    println!(
        "vl 检测器: {}",
        svc_pipeline::compliance_vl::vl_known_ids()
    );
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
