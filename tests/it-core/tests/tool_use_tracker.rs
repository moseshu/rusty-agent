//! R3-6b contracts for per-agent tool-use tracking.

use ra_core::{
    item::{AgentId, CallId},
    state::{
        AgentToolUse, ArgumentFingerprint, TOOL_USE_RECENT_LIMIT, ToolUse, ToolUseAttempt,
        ToolUseEntry, ToolUseTracker,
    },
    tool::{ToolLookupKey, ToolNamespace},
};
use serde_json::json;

fn bare(name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare(name).unwrap())
}

fn namespaced(namespace: &str, name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap())
}

fn attempt(identity: ToolUse, call_id: &str, arguments: serde_json::Value) -> ToolUseAttempt {
    ToolUseAttempt::new(identity, CallId::new(call_id), &arguments)
}

fn entry<'a>(tracker: &'a ToolUseTracker, agent: &AgentId, identity: &ToolUse) -> &'a ToolUseEntry {
    tracker
        .agent(agent)
        .and_then(|agent_use| agent_use.entry(identity))
        .unwrap_or_else(|| panic!("agent `{agent}` 没有 {identity:?} 的记录"))
}

#[test]
fn 两个_agent_各记各的账而不是并进一个计数器() {
    let planner = AgentId::new("planner");
    let executor = AgentId::new("executor");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &planner,
        [attempt(read.clone(), "p-1", json!({ "path": "a" }))],
    );
    tracker.record_turn(
        &executor,
        [
            attempt(read.clone(), "e-1", json!({ "path": "a" })),
            attempt(read.clone(), "e-2", json!({ "path": "a" })),
        ],
    );

    // 键是稳定 `AgentId`。按 agent 的展示名分账会把两个同名 agent 并成一个，而 `AgentSpec`
    // 是**刻意允许**展示名重复的。
    assert_eq!(entry(&tracker, &planner, &read).run_calls(), 1);
    assert_eq!(entry(&tracker, &executor, &read).run_calls(), 2);
    assert_eq!(tracker.repeat_streak(&planner, &read), 1);
    assert_eq!(tracker.repeat_streak(&executor, &read), 2);

    // 没记过的 agent 不是 0 次调用的 agent，两者要能分得开。
    assert!(tracker.agent(&AgentId::new("reviewer")).is_none());
    assert_eq!(
        tracker.repeat_streak(&AgentId::new("reviewer"), &read),
        0,
        "从没跑过的 agent 读连续段应当是 0，不是 panic 也不是别人的值"
    );
}

#[test]
fn 同名不同来源的工具是两个身份() {
    let agent = AgentId::new("main");
    let github = namespaced("mcp.github", "search");
    let gitlab = namespaced("mcp.gitlab", "search");
    let local = bare("search");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(github.clone(), "c-1", json!({ "q": "x" })),
            attempt(gitlab.clone(), "c-2", json!({ "q": "x" })),
            attempt(local.clone(), "c-3", json!({ "q": "x" })),
            attempt(github.clone(), "c-4", json!({ "q": "x" })),
        ],
    );

    // 三个 `search` 是三个身份。按模型可见的名字统计会把它们并成一个，
    // 于是熔断器在两个毫不相干的工具上误报，同时漏掉真正的重复。
    let agent_use = tracker.agent(&agent).unwrap();
    assert_eq!(agent_use.entries().len(), 3);
    assert_eq!(entry(&tracker, &agent, &github).run_calls(), 2);
    assert_eq!(entry(&tracker, &agent, &gitlab).run_calls(), 1);
    assert_eq!(entry(&tracker, &agent, &local).run_calls(), 1);

    // 记录顺序是首次使用的顺序，确定性来自它而不是字典序。
    let order = agent_use
        .entries()
        .iter()
        .map(|entry| entry.identity().clone())
        .collect::<Vec<_>>();
    assert_eq!(order, [github, gitlab, local]);
}

#[test]
fn 参数指纹归一化键序但区分取值() {
    let same_a = ArgumentFingerprint::compute(&json!({ "path": "a.txt", "limit": 10 }));
    let same_b = ArgumentFingerprint::compute(&json!({ "limit": 10, "path": "a.txt" }));
    let nested_a = ArgumentFingerprint::compute(&json!({ "o": { "x": 1, "y": 2 } }));
    let nested_b = ArgumentFingerprint::compute(&json!({ "o": { "y": 2, "x": 1 } }));
    let different = ArgumentFingerprint::compute(&json!({ "path": "b.txt", "limit": 10 }));

    // provider 不承诺键序。按原始字节做指纹，模型正在原地打转时检测器反而会说「换了个调用」。
    assert_eq!(same_a, same_b);
    assert_eq!(nested_a, nested_b);
    assert_ne!(same_a, different);

    // 数组是有序的，重排就是另一个调用。
    assert_ne!(
        ArgumentFingerprint::compute(&json!({ "xs": [1, 2] })),
        ArgumentFingerprint::compute(&json!({ "xs": [2, 1] }))
    );
}

#[test]
fn 连续重复只被同一工具的参数变化打断() {
    let agent = AgentId::new("main");
    let grep = bare("grep");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    // 交替调用两个工具，各自的参数都没变。连续段按**这个身份自己的**调用序列算：
    // 只要「别的工具插进来就归零」，卡在 read/grep 循环里的 agent 会报成两条各长 1 的段，
    // 也就是完全看不见这个环。
    for turn in 0..3 {
        tracker.record_turn(
            &agent,
            [
                attempt(read.clone(), &format!("r-{turn}"), json!({ "path": "a" })),
                attempt(grep.clone(), &format!("g-{turn}"), json!({ "q": "todo" })),
            ],
        );
    }
    assert_eq!(tracker.repeat_streak(&agent, &read), 3);
    assert_eq!(tracker.repeat_streak(&agent, &grep), 3);

    // 换了参数就是换了假设，连续段从这一次重新起算，而累计次数继续走。
    tracker.record_turn(
        &agent,
        [attempt(read.clone(), "r-9", json!({ "path": "b" }))],
    );
    assert_eq!(tracker.repeat_streak(&agent, &read), 1);
    assert_eq!(entry(&tracker, &agent, &read).run_calls(), 4);
    // 这一轮没被点名的工具连续段不动——它没有再重复，也没有换假设。
    assert_eq!(tracker.repeat_streak(&agent, &grep), 3);
}

#[test]
fn 本轮计数每轮重置而全_run_计数累计() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let write = bare("write_file");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(read.clone(), "r-1", json!({ "path": "a" })),
            attempt(read.clone(), "r-2", json!({ "path": "b" })),
            attempt(write.clone(), "w-1", json!({ "path": "c" })),
        ],
    );
    assert_eq!(entry(&tracker, &agent, &read).turn_calls(), 2);
    assert_eq!(tracker.agent(&agent).unwrap().turn_calls(), 3);
    assert!(tracker.used_any_this_turn(&agent));

    tracker.record_turn(
        &agent,
        [attempt(read.clone(), "r-3", json!({ "path": "a" }))],
    );
    assert_eq!(entry(&tracker, &agent, &read).turn_calls(), 1);
    assert_eq!(entry(&tracker, &agent, &read).run_calls(), 3);
    // 这一轮没被点名的工具必须读成 0，而不是停在上一轮的值——`reset_tool_choice`
    // 与 tool-use behavior 问的都是「这一轮」。
    assert_eq!(entry(&tracker, &agent, &write).turn_calls(), 0);
    assert_eq!(entry(&tracker, &agent, &write).run_calls(), 1);

    // 一轮什么都没要：agent 依然在册（它确实跑了一轮），但本轮为空。
    tracker.record_turn(&agent, []);
    assert!(!tracker.used_any_this_turn(&agent));
    assert_eq!(tracker.agent(&agent).unwrap().turn_calls(), 0);
    assert_eq!(tracker.agent(&agent).unwrap().run_calls(), 4);
}

#[test]
fn 叫不出名字的调用也算用过工具() {
    let agent = AgentId::new("main");
    let vanished = ToolUse::Unresolved("vanished".to_owned());
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(vanished.clone(), "c-1", json!({})),
            attempt(ToolUse::Handoff(AgentId::new("reviewer")), "c-2", json!({})),
            attempt(
                ToolUse::Mcp {
                    server: "docs".to_owned(),
                    tool_name: "search".to_owned(),
                },
                "req-1",
                json!({ "q": "x" }),
            ),
        ],
    );

    // `reset_tool_choice` 问的是「模型这轮要了东西吗」。解析不到的名字、控制权转移、
    // 托管审批都是「要了」——只看跑成功的会让强制 tool_choice 一直挂着。
    assert!(tracker.used_any_this_turn(&agent));
    assert_eq!(tracker.agent(&agent).unwrap().entries().len(), 3);
    assert_eq!(entry(&tracker, &agent, &vanished).run_calls(), 1);

    // 反复点同一个不存在的名字同样是个环。
    tracker.record_turn(&agent, [attempt(vanished.clone(), "c-3", json!({}))]);
    assert_eq!(tracker.repeat_streak(&agent, &vanished), 2);
}

#[test]
fn 最近窗口有上限而累计次数与连续段不受它限制() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let total = TOOL_USE_RECENT_LIMIT + 3;
    let mut tracker = ToolUseTracker::new();

    for index in 0..total {
        tracker.record_turn(
            &agent,
            [attempt(
                read.clone(),
                &format!("c-{index}"),
                json!({ "path": "a" }),
            )],
        );
    }

    let read_entry = entry(&tracker, &agent, &read);
    // 无界的轨迹会随 run 变长，并在每次 checkpoint 落盘时整份重写。
    assert_eq!(read_entry.recent().len(), TOOL_USE_RECENT_LIMIT);
    // 被窗口挤掉的那几次仍然算数：总数与连续段是折叠值，不是窗口的投影。
    assert_eq!(read_entry.run_calls(), u32::try_from(total).unwrap());
    assert_eq!(read_entry.repeat_streak(), u32::try_from(total).unwrap());
    // 留下的是最新的几次，最旧的先掉。
    assert_eq!(read_entry.recent()[0].call_id().as_str(), "c-3");
    assert_eq!(
        read_entry.recent().last().unwrap().call_id().as_str(),
        &format!("c-{}", total - 1)
    );
}

#[test]
fn 同一个_call_id_记两次不会把重复度翻倍() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    let turn = || {
        [
            attempt(read.clone(), "c-1", json!({ "path": "a" })),
            attempt(read.clone(), "c-2", json!({ "path": "a" })),
        ]
    };
    tracker.record_turn(&agent, turn());
    // 恢复一个中断就是把同一条响应再结算一遍。重复计数会让模型看起来比实际更爱打转，
    // 而熔断器正是按这个数字动手的。
    tracker.record_turn(&agent, turn());

    let read_entry = entry(&tracker, &agent, &read);
    assert_eq!(read_entry.run_calls(), 2);
    assert_eq!(read_entry.repeat_streak(), 2);
    assert_eq!(read_entry.recent().len(), 2);
    // 本轮计数仍然是这一轮真实要了几次，重放后读到的值和第一次一样。
    assert_eq!(read_entry.turn_calls(), 2);
}

#[test]
fn 超过最近窗口的单轮重放仍然完全幂等() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();
    let total = TOOL_USE_RECENT_LIMIT + 1;
    let turn = || {
        (0..total)
            .map(|index| {
                attempt(
                    read.clone(),
                    &format!("c-{index}"),
                    json!({ "path": "a" }),
                )
            })
            .collect::<Vec<_>>()
    };

    tracker.record_turn(&agent, turn());
    tracker.record_turn(&agent, turn());

    let read_entry = entry(&tracker, &agent, &read);
    assert_eq!(read_entry.run_calls(), u32::try_from(total).unwrap());
    assert_eq!(
        read_entry.repeat_streak(),
        u32::try_from(total).unwrap(),
        "重放不能因窗口淘汰而抬高熔断计数"
    );
    assert_eq!(read_entry.turn_calls(), u32::try_from(total).unwrap());
}

#[test]
fn 序列化往返后继续记账且原始参数不进快照() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();
    tracker.record_turn(
        &agent,
        [attempt(
            read.clone(),
            "c-1",
            json!({ "token": "hunter2-secret-value" }),
        )],
    );

    let encoded = serde_json::to_string(&tracker).unwrap();
    // 工具参数是模型往里塞什么就有什么，而这个结构会进每一次 checkpoint。摘要回答了
    // 消费方唯一真正在问的问题（「和上次是同一个调用吗」），同时把载荷整个拿出快照。
    assert!(
        !encoded.contains("hunter2-secret-value"),
        "参数原文不该出现在快照里：{encoded}"
    );
    assert!(encoded.contains(
        ArgumentFingerprint::compute(&json!({ "token": "hunter2-secret-value" })).as_str()
    ));

    let mut restored: ToolUseTracker = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored, tracker);

    // 恢复后要能接着数。空 tracker 复原会让每次 pause/resume 都清零连续段,
    // 也就是把「暂停再继续」变成绕开熔断器的办法。
    restored.record_turn(
        &agent,
        [attempt(
            read.clone(),
            "c-2",
            json!({ "token": "hunter2-secret-value" }),
        )],
    );
    assert_eq!(restored.repeat_streak(&agent, &read), 2);
    assert_eq!(entry(&restored, &agent, &read).run_calls(), 2);
}

#[test]
fn 更高版本写下的字段原样回写() {
    let stored = json!({
        "schema_version": 9,
        "agents": {
            "main": {
                "schema_version": 9,
                "entries": [{
                    "schema_version": 9,
                    "identity": { "type": "tool", "data": {
                        "schema_version": 1, "kind": "bare", "name": "read_file"
                    }},
                    "run_calls": 2,
                    "turn_calls": 1,
                    "repeat_streak": 2,
                    "recent": [],
                    "cost_cents": 41
                }],
                "quarantined": true
            }
        },
        "sub_runs": ["nested-1"]
    });

    let tracker: ToolUseTracker = serde_json::from_value(stored).unwrap();
    assert_eq!(tracker.schema_version().get(), 9);
    assert_eq!(
        tracker.unknown().get("sub_runs"),
        Some(&json!(["nested-1"]))
    );

    let agent = AgentId::new("main");
    let agent_use = tracker.agent(&agent).unwrap();
    assert_eq!(agent_use.unknown().get("quarantined"), Some(&json!(true)));
    assert_eq!(
        agent_use.entries()[0].unknown().get("cost_cents"),
        Some(&json!(41))
    );
    assert_eq!(agent_use.entries()[0].run_calls(), 2);

    // 「新版本写、旧版本读、旧版本再写」这条路径上一个字段都不能丢。
    let rewritten = serde_json::to_value(&tracker).unwrap();
    assert_eq!(rewritten["sub_runs"], json!(["nested-1"]));
    assert_eq!(rewritten["agents"]["main"]["quarantined"], json!(true));
    assert_eq!(
        rewritten["agents"]["main"]["entries"][0]["cost_cents"],
        json!(41)
    );
}

#[test]
fn 一个身份出现在两条记录里当场被拒() {
    let identity = json!({ "type": "tool", "data": {
        "schema_version": 1, "kind": "bare", "name": "read_file"
    }});
    let stored = json!({
        "schema_version": 1,
        "agents": {
            "main": {
                "schema_version": 1,
                "entries": [
                    { "schema_version": 1, "identity": identity, "run_calls": 3, "recent": [] },
                    { "schema_version": 1, "identity": identity, "run_calls": 4, "recent": [] }
                ]
            }
        }
    });

    // 计数被劈成两半，每个消费方读到先撞上的那一份：熔断器要两倍的重复才会响,
    // 而记录本身看起来完全正常。
    let error = serde_json::from_value::<ToolUseTracker>(stored).unwrap_err();
    assert!(error.to_string().contains("read_file"), "{error}");
}

#[test]
fn tool_use_四个变体都能稳定往返() {
    for identity in [
        bare("read_file"),
        namespaced("mcp.github", "search"),
        ToolUse::Handoff(AgentId::new("reviewer")),
        ToolUse::Mcp {
            server: "docs".to_owned(),
            tool_name: "search".to_owned(),
        },
        ToolUse::Unresolved("vanished".to_owned()),
    ] {
        let encoded = serde_json::to_string(&identity).unwrap();
        let decoded: ToolUse = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, identity, "{encoded} 没能原样回来");
    }

    // deferred 顶层工具与同名裸工具不是一个身份，这条区分要一路撑到快照里。
    let deferred = ToolUse::Tool(ToolLookupKey::deferred_top_level("read_file").unwrap());
    assert_ne!(deferred, bare("read_file"));
    let round_tripped: ToolUse =
        serde_json::from_str(&serde_json::to_string(&deferred).unwrap()).unwrap();
    assert_ne!(round_tripped, bare("read_file"));
}

#[test]
fn agent_视图的合计是投影而不是第二份计数() {
    let agent = AgentId::new("main");
    let mut tracker = ToolUseTracker::new();
    tracker.record_turn(
        &agent,
        [
            attempt(bare("read_file"), "c-1", json!({})),
            attempt(bare("write_file"), "c-2", json!({})),
            attempt(bare("read_file"), "c-3", json!({})),
        ],
    );

    let agent_use: &AgentToolUse = tracker.agent(&agent).unwrap();
    let summed: u32 = agent_use
        .entries()
        .iter()
        .map(ToolUseEntry::run_calls)
        .sum();
    assert_eq!(agent_use.run_calls(), summed);
    assert_eq!(agent_use.turn_calls(), summed);
    assert_eq!(agent_use.used_any_this_turn(), agent_use.turn_calls() > 0);
}
