//! umaai-rs - Rewrite UmaAI in Rust
//!
//! author: curran
//!
//! 职责：CLI 解析、初始化（config / logger / global / trainer）、watch 循环与分发。
//! 具体场景逻辑（温泉 / 拉面）在 `scenario`，决策后处理（luck / 输出）在 `decision`。

use std::{
    sync::Arc,
    time::Instant
};

use anyhow::Result;
use colored::Colorize;
use lexopt::prelude::*;
use log::info;
use rand::{SeedableRng, rngs::StdRng};
use serde::Serialize;
use text_to_ascii_art::to_art;
use umasim::{
    game::Game,
    gamedata::init_global_with_config,
    neural::Evaluator,
    output::{DecisionSink, HumanReadableSink, StdoutJsonSink},
    search::SearchConfig,
    trainer::{MctsTrainer, RamenMctsTrainer},
    utils::{check_working_dir, init_logger, load_game_config}
};

use crate::{
    decision::{LastReasonSink, LuckScoreTracker},
    protocol::urafile::UraFileWatcher,
    scenario::{onsen, ramen}
};

pub mod decision;
pub mod protocol;
pub mod scenario;
pub mod utils;

/// CLI 参数
///
/// `--json`：stdout 严格只 JSON（AIRedirector 模式）；启动横幅 / 日志 / 状态
/// 走 stderr，避免污染 JSON 流。**不引入新命令行参数**——除 `--json` 模式开关
/// 外，所有可调项走 `game_config.toml` / `default_config.toml`（详见集成文档 §3.2.3）。
#[derive(Default)]
struct Args {
    /// `--json` 模式：stdout 仅 JSON，供 AIRedirector 抓取
    json: bool,
    /// `--help` / `-h` 模式：打印用法并退出 0
    help: bool
}

/// 解析 CLI 参数（lexopt 与项目惯例一致——umasim 主 bin 全部用 lexopt）
///
/// 未知参数通过 `arg.unexpected()` 转为 `Err`，不静默接受歧义输入。
fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Long("json") => args.json = true,
            Short('h') | Long("help") => args.help = true,
            _ => return Err(arg.unexpected().into())
        }
    }
    Ok(args)
}

/// 打印 `--help` 输出（两个 sink 都走 stdout 没问题——`-h` 与 `--json` 互斥）
fn print_help_and_exit() -> ! {
    println!("umaai-rs — UmaAI decision engine");
    println!();
    println!("用法: umaai [--json]");
    println!();
    println!("选项:");
    println!("  --json    stdout 严格只 JSON（AIRedirector 模式：启动横幅 / 日志走 stderr）");
    println!("  -h, --help  打印本帮助");
    std::process::exit(0);
}

pub fn run_evaluate<G, E>(game: &G, evaluator: &E, rng: &mut StdRng) -> Result<()>
where
    G: Game + Serialize,
    G::Action: Serialize,
    E: Evaluator<G>
{
    let t = Instant::now();
    let score = evaluator.evaluate(&game);
    if let Some(action) = evaluator.select_action(&game, rng) {
        info!(
            "{}",
            format!(
                "AI选择: {action:?}, 均分: {}, 标准差: {}, Time: {:?}",
                score.score_mean as i64,
                score.score_stdev as i64,
                t.elapsed()
            )
            .bright_green()
        );
    }
    Ok(())
}

/// 实际的主函数
async fn main_guard() -> Result<()> {
    let args = parse_args()?;
    if args.help {
        print_help_and_exit();
    }

    // sink 选择必须在 colored::set_override 之前——后者是全局副作用
    //
    // `--json` 分支额外保留 `StdoutJsonSink` 的具体类型句柄（`json_sink`）：
    // `DecisionSink` trait 只覆盖决策 emit（info/error 不在内）。`emit_info` /
    // `emit_error` 是 `StdoutJsonSink` 的额外方法，main 在 watch loop 的各触发点
    // 显式调——human 模式下 `json_sink` 为 `None`，闭包 no-op。
    let json_sink: Option<Arc<StdoutJsonSink>>;
    let sink: Arc<dyn DecisionSink> = if args.json {
        // JSON 模式关闭 ANSI：colored 即使 --no-color 也可能输出 ANSI reset，
        // 影响 AIRedirector 解析。详见集成文档 §3.2.6 第 3 条。
        colored::control::set_override(false);
        let js = Arc::new(StdoutJsonSink);
        json_sink = Some(js.clone());
        js
    } else {
        json_sink = None;
        Arc::new(HumanReadableSink)
    };
    let json_mode = args.json;

    // info / error 发射器闭包：human 模式 no-op；json 模式转发到 StdoutJsonSink
    // （stdout 严格只 JSON——不再走 eprintln/println 污染流）。闭包按 Fn 借用
    // json_sink，可在 watch loop 内反复调用；同时作为 `&dyn Fn(&str)` 传给场景模块。
    let emit_info = |event: &str| {
        if let Some(ref js) = json_sink {
            js.emit_info(event);
        }
    };
    let emit_error = |message: &str| {
        if let Some(ref js) = json_sink {
            js.emit_error(message);
        }
    };

    // 启动横幅走 stderr（避免污染 JSON 模式的 stdout 流）
    eprintln!("{}", to_art("Ramen-AI".to_string(), "small", 0, 1, 0).expect("here"));
    // 0. 运行前检查（Windows terminal 检测暂时注释掉——非 Windows 平台跳过，
    //    避免误报；Step 5 之后视需要再决定是否启用）
    // check_windows_terminal()?;
    if !fs_err::exists("game_config.toml")? {
        check_working_dir()?;
    }
    // 1. 先读取配置文件
    let game_config = load_game_config()?;
    let mcts_config = SearchConfig::new_game_config(&game_config);
    // 2. 根据配置初始化日志，设置工作线程
    init_logger("umaai", &game_config.log_level)?;
    init_global_with_config(&game_config)?;
    info!(
        "{}",
        format!("工作线程数: {}", game_config.collector.threads).bright_yellow()
    );
    rayon::ThreadPoolBuilder::new()
        .num_threads(game_config.collector.threads)
        .build_global()?;
    //info!("search_config = {mcts_config:?}");

    // 3. 再初始化全局数据
    init_global_with_config(&game_config)?;

    let mut rng = StdRng::from_os_rng();

    // 温泉（onsen）MCTS 训练员
    let mut trainer = MctsTrainer::new(mcts_config).verbose(true);
    trainer.mcts_onsen = game_config.mcts_selected_onsen;
    // 这个设置在AI模式下不生效
    trainer.mcts_selection = "score".to_string();

    // 拉面 MCTS 训练员（与 onsen 的 MctsTrainer 强耦合 OnsenGame 不同；拉面用
    // RamenMctsTrainer 绑 RamenGame，独立构造。stages 走 game_config.mcts.ramen_search_stages，
    // 与 umasim/src/main.rs 拉面路径口径一致。
    //
    // verbose=false：关闭 trainer 内部 `info!("[回合 X] 首选...")` 的 `log::info!` 上屏
    // （避免与下方 human mode 下手动调 `render_reason_lines` 双打印，且
    // umaai 默认关 log，trainer 走 info! 看不到）。DecisionReasonData 通过
    // `with_reason_sink(LastReasonSink)` 缓存到 `reason_slot`。
    let ramen_mcts_config = SearchConfig::new_game_config(&game_config);
    let ramen_stages = umasim::trainer::RamenSearchStages::parse(&game_config.mcts.ramen_search_stages)?;
    let reason_slot = LastReasonSink::new();
    let ramen_trainer = RamenMctsTrainer::new(ramen_mcts_config)
        .with_stages(ramen_stages)
        .verbose(true)
        .with_reason_sink(reason_slot.clone());

    // Phase 4 feature 拆分后，onnx 评估器路径已 cfg gate 到 `onnx` feature。
    // 当前通道层不依赖 onnx（不需要 tract-onnx 巨大依赖链），强制走 MctsTrainer
    // 默认的 handwritten leaf eval（FlatSearch::new() 默认就是 Handwritten）。
    // 后续若恢复 nn leaf，可在此处重新启用 cfg(feature = "onnx") 分支。
    let _rollout_evaluator = game_config.mcts.rollout_evaluator.as_str();
    let _neuralnet_model_path = game_config.neuralnet_model_path.as_str();
    let _max_depth = game_config.mcts.max_depth;
    // 始终强制 handwritten（保持与原 "handwritten" 分支一致的行为）
    trainer.search = trainer.search.with_leaf_evaluator_handwritten();

    // E4：leaf eval 微批大小（batch=1 等价于逐样本推理；batch>1 才会启用 infer_batch）
    trainer.search = trainer
        .search
        .with_rollout_batch_size(game_config.mcts.rollout_batch_size);

    // 开始检测文件——init 失败时优雅退出（不 panic）：路径无效 / notify 失败都打 warn + return Ok(())
    let mut watcher = match UraFileWatcher::init() {
        Ok(w) => {
            // watcher 就绪、即将开始接受游戏数据：--json 模式下通知 AIRed 连接成功。
            // human 模式无需此事件（emit_info 本就 no-op），显式 gate 到 json_mode。
            if json_mode {
                emit_info("connected");
            }
            w
        }
        Err(e) => {
            // watcher init 失败：json 模式发 error 行；human 模式保留原 warn 日志
            emit_error(&format!("watcher 初始化失败: {e}"));
            log::warn!("UraFileWatcher init 失败: {e}，main 不进入 watch loop，程序正常退出（exit 0）");
            return Ok(());
        }
    };

    // Luck score 跟踪器（每回合 baseline 累加 + 切局检测，snapshot 挂到
    // DecisionInfo::scenario_extra 下发给 AIRedirector）。
    let mut luck_tracker = LuckScoreTracker::new();

    loop {
        let contents = watcher.watch("thisTurn.json")?;
        // 收到一份新 JSON：通知 AIRed "开始计算本回合"
        emit_info("compute_start");
        // 按 baseGame.scenarioId 分发（12=温泉 / 14=拉面）到对应场景模块
        match crate::protocol::parse_game_by_scenario(&contents) {
            Ok(crate::protocol::ParsedGame::Onsen(game)) => {
                onsen::process_onsen(
                    game, &mut trainer, &sink, &mut luck_tracker, &mut rng, json_mode, &emit_info, &game_config,
                )?;
            }
            Ok(crate::protocol::ParsedGame::Ramen { mut game, single_mode_chara_id }) => {
                // fanskip：真机路径注入粉丝跳过开关（模拟器/bench 不受影响）
                game.base.skip_free_race_check = game_config.ramen_skip_free_race;
                ramen::process_ramen(
                    game, single_mode_chara_id, &ramen_trainer, &reason_slot, &sink, &mut luck_tracker, &mut rng,
                    json_mode, &emit_info,
                )?;
            }
            Err(e) => {
                // json 模式：发 error JSON 行（不再用 println 污染 stdout 严格 JSON 流）
                // human 模式：保留原 println 红色提示，玩家可见
                emit_error(&format!("解析回合信息出错: {e}"));
                if !json_mode {
                    println!("{}", format!("解析回合信息出错: {e}").red());
                    println!("----------");
                }
            }
        }
    }
}

/// 出错时按 Enter 暂停（仅发布版，CI / 开发默认不阻塞 stdin）
///
/// 与 `release-pause` feature 联动：
/// - `cargo build --release`（默认）：开发 / CI 路径，**不暂停**——避免 stdin 在
///   自动化场景里 hang，且让 cargo run 时 Ctrl-C 后立即退出方便调试。
/// - `cargo build --release --features release-pause`：发布给用户的二进制，
///   启动后暂停"按 Enter 退出"，让用户看清错误信息。
#[cfg(feature = "release-pause")]
fn pause_on_exit() {
    eprintln!("\n按 Enter 退出...");
    let _ = std::io::stdin().read_line(&mut String::new());
}

#[cfg(not(feature = "release-pause"))]
fn pause_on_exit() {
    // 开发 / CI 默认 no-op；详见 fn pause_on_exit 文档
}

#[tokio::main]
async fn main() -> Result<()> {
    match main_guard().await {
        Ok(_) => {}
        Err(e) => {
            println!("{}", "UmaAI 出现错误，即将退出:".red());
            println!("{}", "-----------------------------------".red());
            println!("{}", format!("{e:?}").red());
            pause_on_exit();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{env, path::Path, sync::mpsc};

    use anyhow::Result;
    use colored::Colorize;
    use lexopt::prelude::*;
    use log::info;
    use notify::{Event, RecursiveMode, Watcher};
    use umasim::{gamedata::init_global, utils::init_logger};

    use super::Args;
    use crate::protocol::{
        GameStatusOnsen,
        urafile::{UraFileWatcher, parse_game}
    };

    /// 把 lexopt::Parser + 解析逻辑包成一个 helper（与 `parse_args` 同结构，
    /// 但用 `from_iter` 喂手工 vec 避免依赖真实 env arg）
    fn parse_from<I: IntoIterator<Item = String>>(args: I) -> anyhow::Result<Args> {
        let mut out = Args::default();
        let mut parser = lexopt::Parser::from_iter(args);
        while let Some(arg) = parser.next()? {
            match arg {
                Long("json") => out.json = true,
                Short('h') | Long("help") => out.help = true,
                _ => return Err(arg.unexpected().into())
            }
        }
        Ok(out)
    }

    /// 空参数列表 → 默认 `Args { json: false, help: false }`
    #[test]
    fn test_parse_args_default() -> Result<()> {
        let args = parse_from(vec!["umaai".to_string()])?;
        assert!(!args.json, "默认 json=false");
        assert!(!args.help, "默认 help=false");
        Ok(())
    }

    /// `--json` 解析为 `json=true`
    #[test]
    fn test_parse_args_json() -> Result<()> {
        let args = parse_from(vec!["umaai".to_string(), "--json".to_string()])?;
        assert!(args.json, "--json 触发");
        assert!(!args.help, "help 仍为 false");
        Ok(())
    }

    /// 未知参数 → `Err`，不静默接受歧义输入
    #[test]
    fn test_parse_args_unknown_rejects() {
        let result = parse_from(vec!["umaai".to_string(), "--bogus".to_string()]);
        println!("--bogus 解析: is_err={}", result.is_err());
        assert!(result.is_err(), "未知参数必须报错");
    }

    #[tokio::test]
    async fn test_watch() -> Result<()> {
        let local_app_path = env::var("LOCALAPPDATA")?;
        let urafile_path = format!("{local_app_path}/UmamusumeResponseAnalyzer/PluginData/SendGameStatusPlugin/");

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(tx)?;
        println!("{urafile_path}");
        watcher.watch(Path::new(&urafile_path), RecursiveMode::NonRecursive)?;
        loop {
            let event = rx.recv()??;
            println!("{event:?}");
        }
    }

    #[test]
    fn test_urafile() -> Result<()> {
        // 2. 根据配置初始化日志
        init_logger("test", "info")?;

        // 3. 再初始化全局数据
        init_global()?;
        let mut watcher = UraFileWatcher::init()?;
        loop {
            let contents = watcher.watch("thisTurn.json")?;
            match parse_game::<GameStatusOnsen>(&contents) {
                Ok(game) => {
                    info!("{}", game.explain_distribution()?);
                    println!("----------");
                }
                Err(e) => {
                    println!("{}", format!("解析回合信息出错: {e}").red());
                    println!("----------");
                }
            }
        }
    }
}