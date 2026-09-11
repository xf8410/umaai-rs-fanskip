pub mod action;
pub mod basic;
pub mod person;
use std::{collections::HashSet, default::Default};

pub use action::*;
use anyhow::Result;
use hashbrown::HashMap;
pub use person::*;
use rand::{Rng, seq::IndexedRandom};

use crate::{
    diag,
    explain::Explain,
    game::*,
    gamedata::{ActionValue, ChoiceResult, EventChoice, EventData, TriggerType},
    utils::*
};

/// 一局游戏的基本状态，剧本通用，用于计算，不用于通信(例如通信只传递卡组id)
/// 不包含人头信息(Person类型可能不同)，实际的剧本对象需要补上Vec<Person>才能实现Game Trait
/// 需要频繁clone，一部分不变量需要引用
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BaseGame {
    /// 回合数 [0, 77]
    pub turn: i32,
    /// 回合阶段
    pub stage: TurnStage,
    /// 马娘信息
    pub uma: Uma,
    /// 卡组信息
    pub deck: Vec<SupportCard>,
    /// 继承因子信息，在育成中不变但是要随时取
    pub inherit: InheritInfo,
    /// 友人数据
    pub friend: FriendState,
    /// 设施等级计数 (设施等级x4)
    pub train_level_count: Array5,
    /// 人头分布 [训练, persons_index]. -1为不在
    /// 使用index而非引用
    pub distribution: Vec<Vec<i32>>,
    /// 不在率下降，处理成加算
    pub absent_rate_drop: i32,
    /// 已经触发的事件id和次数
    pub events: HashMap<u32, u32>,
    /// 本回合内还没触发的事件(Hint, 点击友人等)
    pub unresolved_events: Vec<EventData>,
    /// 每种训练卡数量，用于训练倾向和固有判断
    pub card_type_count: [i32; 7],
    /// 友人事件 ID 集合（base/onsen 从 global_events.friend_events 派生；
    /// ramen 在 `RamenGame::newgame` 中额外合并 `RAMENDATA.friend_events`）
    pub friend_event_ids: HashSet<u32>,
    /// 粉丝跳过：跳过自选比赛达标判定（fanskip 仓库新增）
    ///
    /// `true` 时 [`Self::check_free_race`] 不再按"区间内已赛场次"判定育成失败。
    /// 背景：真机游戏对自选比赛的真实判定是粉丝数，而本模拟模型只有场次位图——
    /// 当玩家种马继承粉丝基数高/已自行打满粉丝时，游戏不会失败，但模拟仍按
    /// 场次缺口判死，导致 MCTS 在区间末尾被迫浪费回合打多余的自选比赛。
    ///
    /// 默认 `false`（严格场次判定，模拟器/基准行为不变）。仅真机 AI 路径在
    /// `game_config.toml` 配置 `ramen_skip_free_race = true` 时置位——由用户
    /// 自行保证粉丝达标（上游文档："可以自行不打"）。
    pub skip_free_race_check: bool
}

impl BaseGame {
    /// 该回合是否允许**自选比赛**（通用规则，各剧本 list_actions 复用）
    ///
    /// 自选比赛仅限回合 13-71（0-based）：回合 0-12 太早（出道赛/无赛事），
    /// URA 回合（72-77）不可自选——其中 73/75/77 为生涯决赛（`Uma::is_race_turn`
    /// 短路为唯一动作），72/74/76 为剧本黑障回合（无可选比赛，越界 `race_grades`
    /// 0-71 表）。
    ///
    /// 注意：`BasicGame::list_actions` 以 `turn > 13` 起始（较本规则晚 1 回合，
    /// 历史行为保留），`RamenGame` 使用本方法（从 13 起）。
    pub fn can_self_race(&self) -> bool {
        self.turn > 12 && self.turn < 72
    }

    /// 该回合是否允许**友人出行**（通用规则，各剧本 list_actions 复用）
    ///
    /// 条件：友人已解锁（AfterUnlock）且未进入 URA 回合（72-77）且出行次数未用完。
    pub fn can_friend_outing(&self) -> bool {
        self.friend.out_state == FriendOutState::AfterUnlock
            && self.turn < 72
            && !self.friend.out_used.iter().all(|used| *used)
    }

    pub fn explain(&self) -> Result<String> {
        let mut lines = vec![];
        lines.push(format!(
            "回合: {}-{:?} 设施等级: {} 友人: {}",
            self.turn + 1,
            self.stage,
            Explain::train_level_count(&self.train_level_count),
            self.friend.explain()
        ));
        lines.push(self.uma.explain()?);
        Ok(lines.join("\n"))
    }

    /// 建立游戏对象
    ///
    /// `limit_base` 由调用的剧本提供（见 [`Uma::new`]）。构造顺序是
    /// **先写剧本基值、再加继承**；任何剧本都不得在本函数返回后整体赋值
    /// `five_status_limit`，那会擦掉这里已累加的开局继承增量。
    pub fn new(uma_id: u32, deck_ids: &[u32; 6], inherit: InheritInfo, limit_base: Array5) -> Result<Self> {
        let mut uma = Uma::new(uma_id, limit_base)?;
        diag!("{}", uma.explain()?);
        let mut deck = vec![];
        let mut friend_id = None;
        let mut friend_index = 0;
        let mut card_type_count = [0; 7];
        // 支援卡
        for (index, id) in deck_ids.iter().enumerate() {
            let card = SupportCard::new(*id)?;
            // 初始属性
            let initial = card.initial_bonus();
            let race_bonus = card.effect.saihou;
            if !initial.is_default() {
                diag!("{} +初始属性 {initial:?} 赛后{race_bonus}", card.short_name());
            } else {
                diag!("{} 赛后{race_bonus}", card.short_name());
            }
            let (initial, pt) = split_status(initial)?;
            uma.five_status.add_eq(initial);
            uma.skill_pt += pt;
            uma.race_bonus += race_bonus;
            // 友人. 暂时不处理多个友人
            if card.card_type >= 5 {
                friend_id = Some(*id);
                friend_index = index;
            }
            // 记录训练类型
            if card.card_type < 7 {
                card_type_count[card.card_type as usize] += 1;
            }
            deck.push(card);
        }
        // 继承
        let newgame_inherit = inherit.inherit_newgame();
        let newgame_limit_inherit = inherit.inherit_limit_newgame();
        diag!("+继承: {newgame_inherit:?}");
        uma.five_status.add_eq(&newgame_inherit);
        uma.five_status_limit.add_eq(&newgame_limit_inherit);
        Ok(Self {
            turn: 0,
            stage: TurnStage::Begin,
            uma,
            deck,
            inherit,
            friend: FriendState::new(friend_id, friend_index)?,
            train_level_count: [0; 5],
            distribution: vec![],
            events: HashMap::new(),
            absent_rate_drop: 0,
            unresolved_events: vec![],
            card_type_count,
            // 从 global_events().friend_events.values() 派生友人事件 ID
            // （base/onsen 用；ramen 在 RamenGame::newgame 中额外合并 RAMENDATA.friend_events）
            friend_event_ids: global_events().friend_events.values().map(|e| e.id).collect(),
            // 模拟器/newgame 路径默认严格判定；真机路径由 main.rs 按 game_config 注入
            skip_free_race_check: false
        })
    }

    pub fn base_train_level(&self, train: usize) -> usize {
        (self.train_level_count[train] / 4 + 1).max(0).min(5) as usize
    }
    /// 随机选择一个能发生的事件
    pub fn random_select_event(&self, events: &[EventData], rng: &mut impl Rng) -> Option<EventData> {
        let available_events: Vec<_> = events
            .iter()
            .filter(|e| {
                if let TriggerType::Random { max_time } = &e.trigger {
                    *max_time == 0 || *self.events.get(&e.id).unwrap_or(&0) < *max_time
                } else {
                    true
                }
            })
            .collect();
        available_events.choose(rng).map(|e| (*e).clone())
    }

    /// 已经选择了选项choice，随机决定结果
    pub fn random_select_choice_result(&self, choices: &[EventChoice], rng: &mut impl Rng) -> Option<EventChoice> {
        if choices.is_empty() {
            None
        } else if choices.len() == 1 {
            Some(choices[0].clone())
        } else {
            choices
                .choose_weighted(rng, |c| c.prob)
                .inspect_err(|e| log::error!("sample error: {e:?}"))
                .ok()
                .cloned()
        }
    }
    /// 使事件生效，并随机决定结果, 返回实际生效的效果
    pub fn apply_event(&mut self, event: &EventData, choice: usize, rng: &mut impl Rng) -> Option<EventChoice> {
        self.events.entry(event.id).and_modify(|x| *x += 1).or_insert(1);
        if !event.choices.is_empty() {
            if let Some(mut choice_result) = self.random_select_choice_result(&event.choices[choice], rng) {
                if choice_result.result > 0 {
                    diag!(
                        "事件结果: {}",
                        ChoiceResult::try_from(choice_result.result).unwrap_or_default()
                    );
                }
                // 友人事件：应用 friend.event_bonus / vital_bonus 乘算
                // （base/onsen/ramen 三剧本统一处理；详见 FriendState 字段语义）
                if self.friend_event_ids.contains(&event.id) {
                    Self::apply_friend_bonus(&mut choice_result.value, &self.friend);
                }
                self.uma.add_value(&choice_result.value);
                Some(choice_result)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// 对 ActionValue 应用友人卡词条 bonus 乘算
    ///
    /// - `event_bonus`（支援卡「事件效果提高」词条）：仅对 `status_pt[0..6]`（五维 + pt）乘算，
    ///   公式 `status_pt[i] = status_pt[i] * (100 + event_bonus) / 100`（floor 除法）。
    ///   不影响 vital / max_vital / motivation / hint_level / friendship。
    /// - `vital_bonus`（支援卡「恢复量提高」词条）：仅对 `vital > 0` 乘算，
    ///   公式 `vital = vital * (100 + vital_bonus) / 100`（floor 除法）。
    ///   仅正向体力恢复生效，负向体力消耗不受影响。
    fn apply_friend_bonus(value: &mut ActionValue, friend: &FriendState) {
        if friend.event_bonus != 0 {
            let multiplier = 100 + friend.event_bonus;
            for i in 0..6 {
                if value.status_pt[i] != 0 {
                    value.status_pt[i] = value.status_pt[i] * multiplier / 100;
                }
            }
        }
        if friend.vital_bonus != 0 && value.vital > 0 {
            let multiplier = 100 + friend.vital_bonus;
            value.vital = value.vital * multiplier / 100;
        }
    }

    /// 结算某个事件选项
    pub fn apply_event_choice(&mut self, choice: &EventChoice) {
        self.uma.add_value(&choice.value);
        self.uma.update_flags(choice);
    }

    pub fn is_xiahesu(&self) -> bool {
        (self.turn >= 36 && self.turn < 40) || (self.turn >= 60 && self.turn < 64)
    }

    pub fn generate_card_event(&self, person_index: i32, rng: &mut impl Rng) -> Option<EventData> {
        // 支援卡事件. 再精细一点模拟 后一段事件发生次数不能多于前一段事件
        let card_event_times = [8001, 8002, 8003].map(|id| *self.events.get(&id).unwrap_or(&0));
        let mut available_events = vec![];
        if card_event_times[0] < 5 {
            available_events.push(0);
        }
        if card_event_times[1] < card_event_times[0] {
            available_events.push(1);
        }
        if card_event_times[2] < card_event_times[1] {
            available_events.push(2);
        }
        if let Some(index) = available_events.choose(rng) {
            let mut event = global_events().card_events[*index].clone();

            event.person_index = Some(person_index);
            Some(event)
        } else {
            None
        }
    }

    /// 检测自选比赛是否达标
    ///
    /// fanskip 修改：`skip_free_race_check == true` 时直接通过（粉丝跳过模式）。
    /// 真实游戏按粉丝数判定自选比赛是否过关；玩家粉丝已达标时（种马继承/已自行
    /// 比赛），即使场次位图不足游戏也不会失败。此时跳过判定可避免 MCTS 为躲避
    /// 模拟中的"假失败"而被迫打多余比赛。默认 `false` 保持原严格行为。
    pub fn check_free_race(&self) -> bool {
        if self.skip_free_race_check {
            diag!("粉丝跳过模式：跳过自选比赛达标判定（skip_free_race_check = true）");
            return true;
        }
        if let Ok(data) = self.uma.get_data() {
            for free_race in &data.free_races {
                // 只在结束回合+1时检测
                if self.turn as u32 == free_race.end_turn + 1 {
                    let count = self.uma.count_free_race(free_race);
                    diag!(
                        "回合 {} -> {} 已比赛 {} / {} 场",
                        free_race.start_turn,
                        free_race.end_turn,
                        count,
                        free_race.count
                    );
                    if count < free_race.count {
                        diag!("自选比赛未达标，寄了");
                        return false;
                    }
                }
            }
            true
        } else {
            true
        }
    }

    /// 比较self是否为last的同一或者下一回合
    pub fn is_next_of(&self, last: &BaseGame) -> bool {
        // 马娘ID相同且回合数相同或差1，则再检查卡组
        if self.uma.uma_id == last.uma.uma_id && (self.turn == last.turn || self.turn == last.turn + 1) {
            for i in 0..6 {
                if self.deck[i].card_id != last.deck[i].card_id {
                    return false;
                }
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;
    use crate::{
        game::{base::basic::BasicGame, onsen::game::OnsenGame, ramen::RamenGame},
        gamedata::{*, onsen::ONSENDATA, ramen::RAMENDATA},
        utils::{Checks, get_workspace_root, init_test_logger}
    };

    #[test]
    fn test_explain() -> Result<()> {
        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        init_test_logger("info")?;
        init_global()?;
        let mut game = BaseGame::default();
        game.uma.uma_id = 101901;
        game.uma.motivation = 5;
        game.uma.flags.qiezhe = true;
        println!("{}", game.explain()?);

        Ok(())
    }

    /// 开局保留继承和卡组计数，克隆后的修改不影响原局。
    #[test]
    fn test_newgame() -> Result<()> {
        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        init_test_logger("info")?;
        init_global()?;
        let inherit = InheritInfo {
            blue_count: [15, 3, 0, 0, 0],
            extra_count: [0, 30, 0, 0, 30, 30]
        };
        let game = BaseGame::new(101901, &[302424, 302464, 302484, 302564, 302574, 302644], inherit.clone(),
            global!(GAMECONSTANTS).five_status_limit_base)?;
        println!("{}", game.explain()?);
        let score = game.uma.calc_score();
        println!("评分: {} {}", global!(GAMECONSTANTS).get_rank_name(score), score);

        let mut checks = Checks::new();
        checks.check(game.inherit == inherit, "开局保留全部继承因子");
        checks.check(game.card_type_count == [2, 0, 1, 1, 1, 1, 0], "卡组包含两速、一力、一根、一智、一友人");
        let mut branch = game.clone();
        checks.check(branch == game, "克隆保留完整基础状态");
        branch.inherit.blue_count[0] += 1;
        branch.inherit.extra_count[5] += 1;
        branch.card_type_count[0] -= 1;
        println!("副本继承: {:?}，卡组计数: {:?}", branch.inherit, branch.card_type_count);
        checks.check(game.inherit == inherit, "修改副本的两组继承因子不影响原局");
        checks.check(game.card_type_count == [2, 0, 1, 1, 1, 1, 0], "修改副本的卡组计数不影响原局");
        checks.finish()
    }

    // ========== 通用规则：自选比赛 / 友人出行 ==========

    /// 自选比赛边界：回合 12 及以前禁止；13-71 允许；72+（URA）禁止
    #[test]
    fn test_can_self_race_bounds() -> Result<()> {
        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        let _ = init_test_logger("error");
        let _ = init_global();

        let mut game = BaseGame::default();
        for (turn, expect) in [
            (0, false),
            (12, false),
            (13, true),
            (14, true),
            (71, true),
            (72, false),
            (73, false),
            (74, false),
            (75, false),
            (76, false),
            (77, false)
        ] {
            game.turn = turn;
            let got = game.can_self_race();
            println!("turn={turn} can_self_race={got} (期望 {expect})");
            assert_eq!(got, expect);
        }
        Ok(())
    }

    /// 友人出行边界：未解锁 / URA 回合（72+） / 出行次数用完 → 禁止
    #[test]
    fn test_can_friend_outing_bounds() -> Result<()> {
        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        let _ = init_test_logger("error");
        let _ = init_global();

        let mut game = BaseGame::default();
        // 与 FriendState::new 一致：out_used 长度 5（空 vec 时 all() 真空真，语义不适用）
        game.friend.out_used = vec![false; 5];
        game.friend.out_state = FriendOutState::AfterUnlock;
        game.turn = 70;
        println!("解锁+回合70: {}", game.can_friend_outing());
        assert!(game.can_friend_outing());

        // 未解锁
        game.friend.out_state = FriendOutState::UnClicked;
        println!("未解锁+回合70: {}", game.can_friend_outing());
        assert!(!game.can_friend_outing());

        // URA 回合
        game.friend.out_state = FriendOutState::AfterUnlock;
        game.turn = 72;
        println!("解锁+回合72: {}", game.can_friend_outing());
        assert!(!game.can_friend_outing());

        // 出行次数用完
        game.turn = 70;
        game.friend.out_used = vec![true; 5];
        println!("解锁+回合70+出行用完: {}", game.can_friend_outing());
        assert!(!game.can_friend_outing());

        Ok(())
    }

    // ========== 友人事件效果加成/恢复量加成测试 ==========

    /// 验证 `event_bonus` 对 `status_pt` 的乘算（floor 除法）
    #[test]
    fn test_apply_friend_bonus_status_pt() -> Result<()> {
        // 9 * 130 / 100 = 11
        let mut value = ActionValue {
            status_pt: [10, 0, 0, 9, 0, 20],
            ..Default::default()
        };
        let mut friend = FriendState::default();
        friend.event_bonus = 30;

        BaseGame::apply_friend_bonus(&mut value, &friend);

        println!("event_bonus=30 status_pt=[10,0,0,9,0,20] -> {:?}", value.status_pt);
        // 10 * 130 / 100 = 13; 9 * 130 / 100 = 11; 20 * 130 / 100 = 26
        assert_eq!(value.status_pt, [13, 0, 0, 11, 0, 26]);
        Ok(())
    }

    /// 验证 `vital_bonus` 对正向 `vital` 的乘算
    #[test]
    fn test_apply_friend_bonus_vital() -> Result<()> {
        let mut value = ActionValue {
            vital: 25,
            ..Default::default()
        };
        let mut friend = FriendState::default();
        friend.vital_bonus = 50;

        BaseGame::apply_friend_bonus(&mut value, &friend);

        println!("vital_bonus=50 vital=25 -> vital={}", value.vital);
        // 25 * 150 / 100 = 37
        assert_eq!(value.vital, 37);
        Ok(())
    }

    /// 验证 `event_bonus` 和 `vital_bonus` 都不影响 max_vital / motivation / hint_level / friendship
    #[test]
    fn test_apply_friend_bonus_other_fields_unchanged() -> Result<()> {
        let mut value = ActionValue {
            status_pt: [10, 5, 3, 0, 0, 20],
            vital: 25,
            max_vital: 4,
            motivation: 1,
            hint_level: 2,
            friendship: 15
        };
        let mut friend = FriendState::default();
        friend.event_bonus = 50;
        friend.vital_bonus = 30;

        BaseGame::apply_friend_bonus(&mut value, &friend);

        println!("event_bonus=50 vital_bonus=30 -> {:?}", value);
        // status_pt: 10*150/100=15, 5*150/100=7, 3*150/100=4, 20*150/100=30
        assert_eq!(value.status_pt, [15, 7, 4, 0, 0, 30]);
        // vital: 25 * 130 / 100 = 32
        assert_eq!(value.vital, 32);
        // max_vital / motivation / hint_level / friendship 不受加成影响
        assert_eq!(value.max_vital, 4);
        assert_eq!(value.motivation, 1);
        assert_eq!(value.hint_level, 2);
        assert_eq!(value.friendship, 15);
        Ok(())
    }

    /// 验证 bonus 全为 0 时 value 原样不变（向后兼容）
    #[test]
    fn test_apply_friend_bonus_no_bonus() -> Result<()> {
        let mut value = ActionValue {
            status_pt: [10, 5, 3, 0, 0, 20],
            vital: 25,
            max_vital: 4,
            motivation: 1,
            hint_level: 2,
            friendship: 15
        };
        let friend = FriendState::default(); // event_bonus = 0, vital_bonus = 0

        BaseGame::apply_friend_bonus(&mut value, &friend);

        println!("no bonus -> {:?}", value);
        assert_eq!(value.status_pt, [10, 5, 3, 0, 0, 20]);
        assert_eq!(value.vital, 25);
        assert_eq!(value.max_vital, 4);
        assert_eq!(value.motivation, 1);
        assert_eq!(value.hint_level, 2);
        assert_eq!(value.friendship, 15);
        Ok(())
    }

    /// 验证 `vital_bonus` 仅对正向体力恢复生效，不影响负向体力消耗
    #[test]
    fn test_apply_friend_bonus_vital_negative_not_affected() -> Result<()> {
        let mut value = ActionValue {
            vital: -10,
            ..Default::default()
        };
        let mut friend = FriendState::default();
        friend.vital_bonus = 50;

        BaseGame::apply_friend_bonus(&mut value, &friend);

        println!("vital=-10 with vital_bonus=50 -> vital={}", value.vital);
        // 负向体力消耗不被 vital_bonus 影响
        assert_eq!(value.vital, -10);
        Ok(())
    }

    /// 验证 `BaseGame::apply_event` 路径应用 friend bonus 的集成测试
    #[test]
    fn test_apply_event_friend_bonus_integration() -> Result<()> {
        use rand::SeedableRng;

        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        init_test_logger("info")?;
        init_global()?;

        // 构造带友人卡的 BaseGame（302574 是 hotaku 友人）
        let mut game = BaseGame::new(101901, &[302424, 302464, 302484, 302564, 302574, 302644], InheritInfo {
            blue_count: [15, 3, 0, 0, 0],
            extra_count: [0, 30, 0, 0, 30, 30]
        }, global!(GAMECONSTANTS).five_status_limit_base)?;
        // 强制设置可预测的 friend bonus
        game.friend.event_bonus = 30;
        game.friend.vital_bonus = 20;

        // 取 base 剧本的友人事件 first（id=809050001, status_pt=[0,0,9,9,9,0]）
        let friend_event = global_events().friend_events["first"].clone();
        println!(
            "友人事件 first: id={}, choices={:?}",
            friend_event.id, friend_event.choices
        );
        // 验证 ID 已被 BaseGame::new 自动加入
        assert!(
            game.friend_event_ids.contains(&friend_event.id),
            "friend_event.id={} 应该已加入 friend_event_ids",
            friend_event.id
        );

        // 记录初始状态
        let init_status = game.uma.five_status;
        let init_skill_pt = game.uma.skill_pt;
        let init_vital = game.uma.vital;

        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        game.apply_event(&friend_event, 0, &mut rng);

        println!(
            "应用前 status={:?} skill_pt={} vital={}",
            init_status, init_skill_pt, init_vital
        );
        println!(
            "应用后 status={:?} skill_pt={} vital={}",
            game.uma.five_status, game.uma.skill_pt, game.uma.vital
        );

        // first 事件 value: status_pt=[0,0,9,9,9,0], motivation=1, friendship=10, max_vital=4
        // event_bonus=30 乘算: 9*130/100 = 11
        // 期望: five_status[2]+=11, five_status[3]+=11, five_status[4]+=11
        //       skill_pt += 0（status_pt[5]=0）
        //       vital += 0（vital 未在 first 中）
        assert_eq!(game.uma.five_status[0] - init_status[0], 0);
        assert_eq!(game.uma.five_status[1] - init_status[1], 0);
        assert_eq!(
            game.uma.five_status[2] - init_status[2],
            11,
            "根性应该 +11 (9 * 130 / 100)"
        );
        assert_eq!(
            game.uma.five_status[3] - init_status[3],
            11,
            "智力应该 +11 (9 * 130 / 100)"
        );
        assert_eq!(
            game.uma.five_status[4] - init_status[4],
            11,
            "pt=0 不变（其实是五维第4个，pt 是 status_pt[5]）"
        );
        assert_eq!(game.uma.skill_pt - init_skill_pt, 0, "pt=0 不变");
        Ok(())
    }

    /// 验证友人卡词条无加成时（即 friend.event_bonus=0, vital_bonus=0），
    /// apply_event 行为与现状一致（向后兼容）
    #[test]
    fn test_apply_event_no_friend_bonus_backward_compatible() -> Result<()> {
        use rand::SeedableRng;

        let workspace_root = get_workspace_root()?;
        std::env::set_current_dir(workspace_root)?;
        init_test_logger("info")?;
        init_global()?;

        // 不带友人卡的卡组：5 张普通支援卡 + 1 张 type<5 支援卡（没有友人）
        // 实际 card_id=302424 不带友人（card_type<5）
        // 改用一组全部非友人的卡组来保证 friend.event_bonus=0
        // 这里直接用不带友人的卡组：[302424, 302464, 302484, 302564, 302644, 302694]
        let mut game = BaseGame::new(101901, &[302424, 302464, 302484, 302564, 302644, 302694], InheritInfo {
            blue_count: [15, 3, 0, 0, 0],
            extra_count: [0, 30, 0, 0, 30, 30]
        }, global!(GAMECONSTANTS).five_status_limit_base)?;
        // 验证 friend.event_bonus 和 vital_bonus 都是 0
        assert_eq!(game.friend.event_bonus, 0);
        assert_eq!(game.friend.vital_bonus, 0);

        // 取 base 剧本的友人事件 first
        let friend_event = global_events().friend_events["first"].clone();
        let init_status = game.uma.five_status;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        game.apply_event(&friend_event, 0, &mut rng);

        // 期望 status_pt 原样应用: status_pt=[0,0,9,9,9,0]
        assert_eq!(game.uma.five_status[2] - init_status[2], 9);
        assert_eq!(game.uma.five_status[3] - init_status[3], 9);
        assert_eq!(game.uma.five_status[4] - init_status[4], 9);
        Ok(())
    }

    /// 三个剧本的开局五维上限都必须是「本剧本基值 + 开局继承」，不得被任何常量截断
    ///
    /// 守两类历史缺陷：
    /// 1. 硬编码 `min(2800)`——温泉时代的防御值，速度基值提高后变成硬截断；
    /// 2. 剧本在 `BaseGame::new` 之后**整体赋值** `five_status_limit`，
    ///    把这里已累加的 `inherit_limit_newgame` 增量擦掉。
    ///
    /// 期望值直接用各剧本 JSON 的基值，因此改坏代码会红、改动剧本数据不会误报。
    #[test]
    fn test_newgame_status_limit_is_scenario_base_plus_inherit() -> Result<()> {
        std::env::set_current_dir(get_workspace_root()?)?;
        let _ = init_test_logger("error");
        let _ = init_global();

        let inherit = InheritInfo { blue_count: [15, 3, 0, 0, 0], extra_count: [0, 30, 0, 0, 30, 30] };
        let newgame_inherit = inherit.inherit_limit_newgame();
        let basic_deck = [302424, 302464, 302484, 302564, 302574, 302644];
        let ramen_deck = [302424, 302894, 303044, 302924, 303024, 303054];

        let cases: [(&str, Array5, Array5); 3] = [
            (
                "basic",
                BasicGame::newgame(101901, &basic_deck, inherit.clone())?.uma.five_status_limit,
                global!(GAMECONSTANTS).five_status_limit_base
            ),
            (
                "onsen",
                OnsenGame::newgame(101901, &basic_deck, inherit.clone())?.uma.five_status_limit,
                global!(ONSENDATA).status_limit_base()
            ),
            (
                "ramen",
                RamenGame::newgame(102601, &ramen_deck, inherit.clone())?.uma.five_status_limit,
                global!(RAMENDATA).status_limit_base()
            )
        ];

        let mut c = Checks::new();
        for (name, got, base) in cases {
            for i in 0..5 {
                let want = base[i] + newgame_inherit[i];
                println!("{name} 维度 {i}: 剧本基值 {} + 开局继承 {} = {want}，实际 {}", base[i], newgame_inherit[i], got[i]);
                c.check(got[i] == want, &format!("{name} 维度 {i} 上限 = {want}"));
            }
            c.check(got[0] > base[0], &format!("{name} 速度上限必须含开局继承增量"));
            c.check(got[0] > 2800, &format!("{name} 速度上限不得被 2800 截断"));
        }
        c.finish()
    }
}
