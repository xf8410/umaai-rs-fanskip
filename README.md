<div align="center">

# 🍜 umaai-rs-fanskip

**umaai-rs 粉丝跳过版：真机 AI 不再为"假自选比赛义务"浪费回合**

![上游](https://img.shields.io/badge/上游-xulai1001%2Fumaai--rs-8B5CF6?style=flat-square) ![修改点](https://img.shields.io/badge/修改点-4处%2F45行-10B981?style=flat-square) ![基准](https://img.shields.io/badge/模拟器基准-不变-F59E0B?style=flat-square) ![CI](https://img.shields.io/badge/CI-build--and--test-3B82F6?style=flat-square)

</div>

---

> 📌 **一句话定位**：基于 [xulai1001/umaai-rs](https://github.com/xulai1001/umaai-rs) 的独立改造仓——加一个 `ramen_skip_free_race` 开关，让真机 AI 在粉丝已达标时跳过自选比赛强制判定。

## 🧭 为什么要有这个仓库

上游 umaai-rs 的"已知问题"之一：**粉丝数满足后，AI 依然选择比赛**（上游文档标注"属于正常现象，可以自行不打"）。

本仓库定位并修复了它的根源：

| 层面 | 判定依据 | 问题 |
|---|---|---|
| **真实游戏** | **粉丝数**——进入粉丝数回合区间前已达标，就不需要再打自选比赛 | — |
| **umaai-rs 模拟模型** | **场次**——`FreeRaceData` 只有"区间回合 + 要求场数"，全链路（`check_free_race` 失败判定 / MCTS 终局价值）没有粉丝数概念 | 模型与游戏不一致 |
| **后果** | 种马继承粉丝基数高、粉丝已达标时，模拟仍按场次缺口判"育成失败"，MCTS 为躲避**模拟中的假失败**，在区间末尾被迫浪费回合打多余的比赛 | AI 摆烂少训练 |

## ✨ 修改内容（最小 diff：4 处 / +45 行）

| # | 文件 | 修改 |
|---|---|---|
| 1 | `crates/umasim/src/game/base/mod.rs` | `BaseGame` 新增 `skip_free_race_check` 字段（默认 `false`）；`check_free_race` 开头短路跳过判定（带 diag 日志） |
| 2 | `crates/umasim/src/gamedata/config.rs` | `GameConfig` 新增 `ramen_skip_free_race` 配置（serde default，兼容旧配置文件） |
| 3 | `crates/umaai/src/main.rs` | 真机拉面分支注入开关到 `game.base.skip_free_race_check` |
| 4 | `game_config.toml` | 配置示例与风险说明 |

**行为边界**：

- ✅ **默认关闭**（`ramen_skip_free_race = false`）：与上游行为逐位一致
- ✅ **模拟器 / bench 基准不变**：开关只在真机 watch 循环注入，`newgame` / bench / NN trainer 路径全部维持严格场次判定
- ✅ **游戏内比赛动作保留**：跳过的是"强制义务"，比赛动作仍在候选列表里，MCTS 仍可按收益自主选择

## 🚀 用法

`game_config.toml` 顶部（**必须写在所有 `[xxx]` 段之前**）：

```toml
ramen_skip_free_race = true
```

开启后：

- AI 不再在粉丝数回合区间末尾强制打自选比赛
- **粉丝达标由你自行保证**（看小黑板粉丝数）——种马继承粉丝高、或你已手动打够粉丝时开启；粉丝没达标时请关闭，否则自选比赛未达标会翻车

## 📜 与上游的关系

- 本仓库为独立改造仓，**跟随上游 master**（e8f6f26，2026-09），不向上游发 PR
- 上游修复此问题后（如引入粉丝数字段），本仓库可整体废弃
- 修改全部带 `fanskip` 标记注释，diff 一目了然

## ⚙️ CI

| 流水线 | 用途说明 |
|---|---|
| `build-and-test` | cargo check + cargo test 全 workspace + release 构建，二进制 artifact 30 天 |

## ⚠️ 风险与时效性声明

- 本仓库是**实验性改造**：跳过判定后，AI 决策建立在自己对局面的权衡上，粉丝是否达标请自行确认
- 与上游的差异仅限上述 4 处；上游演进后需要手动 rebase 同步
- 用于其他操作方式，风险自己承担

</div>
