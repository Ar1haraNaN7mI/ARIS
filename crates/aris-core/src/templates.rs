//! Workflow templates contain editable role assignments, dependencies and fixed profiles.
use crate::harness::{TeamAgent, TeamSpec};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplate {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub builtin: bool,
    pub goal: String,
    pub agents: Vec<TeamAgent>,
    #[serde(default)]
    pub files: Vec<String>,
}

fn stage(
    id: &str,
    name: &str,
    role: &str,
    task: &str,
    dependencies: &[&str],
    files: &[&str],
) -> TeamAgent {
    serde_json::from_value(serde_json::json!({"id":id,"name":name,"role":role,"prompt":task,"profile_id":"gpt","depends_on":dependencies,"files":files})).expect("static template")
}
fn template(
    id: &str,
    name: &str,
    category: &str,
    goal: &str,
    final_author: Option<&str>,
    mut agents: Vec<TeamAgent>,
) -> TeamTemplate {
    let outputs = agents
        .iter()
        .map(|agent| (agent.id.clone(), agent.files.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut files = Vec::new();
    for agent in &mut agents {
        if final_author == Some(agent.id.as_str()) {
            agent.delivery = Some(crate::synthesis::DeliveryKind::Final);
        }
        let inputs = agent
            .depends_on
            .iter()
            .flat_map(|id| outputs[id].iter().cloned())
            .collect::<Vec<_>>();
        let input_instruction = if inputs.is_empty() {
            "先阅读用户提供的输入材料与工作区相关上下文。".to_string()
        } else {
            format!("先读取前序产物：{}。", inputs.join("、"))
        };
        agent.prompt = format!("{input_instruction}\n{}\n将本阶段交付写入这些分配文件：{}。完成时列出实际生成的路径、验证结果和未完成项。", agent.prompt, agent.files.join("、"));
        for file in &agent.files {
            if !files.contains(file) {
                files.push(file.clone());
            }
        }
    }
    TeamTemplate {
        id: format!("builtin-{id}"),
        name: name.into(),
        description: goal.into(),
        category: category.into(),
        builtin: true,
        goal: goal.into(),
        agents,
        files,
    }
}
pub fn builtins() -> Vec<TeamTemplate> {
    vec![
        template("research", "资料调研与事实核查", "研究", "并行搜集一手资料与反例，经交叉核查后交付有来源的研究报告", Some("report"), vec![
            stage("sources", "一手资料研究员", "核实原始来源", "围绕主题拆出研究问题，优先查官方文档、原始数据与论文；记录URL、标题、发布日期、访问日期、支持的主张和证据摘录。资料缺失时明确记录。", &[], &["artifacts/research/sources.md"]),
            stage("counterpoints", "独立反证研究员", "寻找反例与适用边界", "独立检索与主题相关的反例、失败案例、过时结论和冲突数据；记录可复查URL与日期，区分事实、观点和未知项。", &[], &["artifacts/research/counterpoints.md"]),
            stage("factcheck", "事实核查员", "对齐证据与结论", "交叉核对两组材料；为关键主张建立证据表，核对时间、定义、样本与来源冲突。必要时重新打开原文；不能确认的主张标记待核实。", &["sources", "counterpoints"], &["artifacts/research/claims.md"]),
            stage("report", "研究报告作者", "形成可追溯结论", "依据已核实的证据回答研究问题，写清结论、适用范围、分歧、引用链接和待解决问题；提出有依据的下一步。", &["sources", "counterpoints", "factcheck"], &["artifacts/research/report.md"]),
        ]),
        template("competitors", "竞品与市场分析", "运营", "并行核对产品能力与价格，再形成统一口径的比较与产品建议", Some("recommend"), vec![
            stage("capabilities", "产品研究员", "验证能力与用户流程", "明确目标产品、受众与核心使用场景；按同一维度核对各产品官网、文档和可访问界面，保留来源与截图路径，区分已体验和仅宣传的能力。", &[], &["artifacts/competitors/capabilities.md"]),
            stage("pricing", "商业模式研究员", "核对价格与实际限制", "独立核对各产品公开价格、计费单位、额度、地区、免费限制和企业条款；记录币种、日期、URL，缺少公开价格时标明而非估算。", &[], &["artifacts/competitors/pricing.md"]),
            stage("compare", "竞争分析师", "统一口径比较", "将能力与定价按目标用户场景对齐，制作比较表；注明不可比项目、证据差距和各方案代价，不能用未验证宣传直接打分。", &["capabilities", "pricing"], &["artifacts/competitors/comparison.md"]),
            stage("recommend", "产品策略负责人", "提出可执行选择", "依据比较表提出定位机会、优先事项和验证实验；每项建议注明受益用户、依据、成本假设、衡量指标与停止条件。", &["compare"], &["artifacts/competitors/recommendations.md"]),
        ]),
        template("content", "内容策划与编辑", "内容", "从受众与渠道简报出发，交付经事实与语言校对的完整内容", Some("edit"), vec![
            stage("brief", "内容策划", "确定受众和表达目标", "梳理用户给出的品牌、产品事实、受众问题、渠道、篇幅与转化目标；列出信息缺口、可引用素材、内容结构和不可擅自承诺的事项。", &[], &["artifacts/content/brief.md"]),
            stage("draft", "内容作者", "完成可编辑初稿", "按简报撰写完整初稿，提供3个标题角度、开头和明确行动入口；事实主张附来源，必要素材缺失处标注待补。", &["brief"], &["artifacts/content/draft.md"]),
            stage("edit", "主编", "校对并交付渠道版本", "复核初稿事实、语气、结构和渠道限制，重写为最终稿；包含标题备选及用户指定的渠道版本，并单列仍需确认的事实或素材。", &["brief", "draft"], &["artifacts/content/final.md", "artifacts/content/editor-notes.md"]),
        ]),
        template("data", "数据分析与复核", "数据", "审计真实数据、运行可复现计算，并独立复核关键结果", None, vec![
            stage("audit", "数据审计员", "明确数据质量与口径", "读取实际数据、字段与样本，核对缺失、重复、单位、时间范围和异常；记录输入路径、业务问题、可用指标与排除规则，保留原始数据。", &[], &["artifacts/data/audit.md"]),
            stage("analyze", "分析工程师", "实现并执行分析", "依据审计写出可运行的Python分析脚本，支持显式输入和输出路径；实际运行并生成结构化指标JSON与分析报告。报告注明命令、数据范围、计算口径和限制；缺输入或依赖时记录失败，不编造结果。", &["audit"], &["artifacts/data/analyze.py", "artifacts/data/metrics.json", "artifacts/data/analysis.md"]),
            stage("reproduce", "独立复核员", "复算关键指标", "阅读脚本和结果，在临时输出目录重跑计算，独立复算关键汇总并检查单位、分母和趋势；不覆盖前序产物。记录实际命令、差异、未复现部分和是否支持报告结论。", &["audit", "analyze"], &["artifacts/data/verification.md"]),
        ]),
        template("operations", "运营活动与复盘", "运营", "并行制定活动执行计划与衡量方案，汇合为可落地的运营包", Some("launch"), vec![
            stage("brief", "活动负责人", "梳理目标与约束", "明确目标、受众、预算输入、渠道、周期、可用资源和交付范围；区分已知条件与待确认假设，不擅自承诺预算或负责人。", &[], &["artifacts/operations/brief.md"]),
            stage("schedule", "执行策划", "制定活动步骤与依赖", "拆分预热、上线、持续运营和收尾任务；为每项给出相对日期、负责人角色、输入素材、依赖、完成标准和中断处理办法。", &["brief"], &["artifacts/operations/schedule.md"]),
            stage("measurement", "运营分析师", "制定指标与复盘口径", "围绕活动目标建立指标定义、分母、数据来源、采集频率、对照方式和复盘问题；目标数值缺输入时列为待确认假设，提供可填写的记录表。", &["brief"], &["artifacts/operations/measurement.md"]),
            stage("launch", "交付协调员", "整合上线与复盘材料", "对齐排期与衡量口径，排除任务依赖冲突和漏项；交付按时间排序的执行清单、素材清单、验收标准、待确认事项及复盘模板。", &["schedule", "measurement"], &["artifacts/operations/runbook.md"]),
        ]),
        template("coding", "工程实现与验收", "工程", "按规划→实现→验收推进；启动前将真实源码文件加入实现角色的输出文件列表", None, vec![
            stage("plan", "工程规划", "阅读上下文并界定实现", "先读项目说明、现有实现、调用方和真实依赖接口；写出精确修改路径、边界条件、验收要求和适量验证命令。明确实现角色需要获分配的源码文件。", &[], &["artifacts/coding/plan.md"]),
            stage("implement", "实现工程师", "完成代码改动", "按计划修改本阶段已分配的真实源码文件，保留用户已有改动，运行相关检查；同时保存本次变更的可应用patch与实现记录。若分配列表只有报告和patch，则交付patch并明确源码尚未修改，不宣称完成实施。", &["plan"], &["artifacts/coding/changes.patch", "artifacts/coding/implementation.md"]),
            stage("verify", "验收工程师", "独立验证实际行为", "检查实际工作树与patch是否一致，依据验收条件执行构建、测试或复现步骤并记录输出。仅有patch时执行适用性检查并标记未部署、未运行的行为；列出具体缺陷、通过项和剩余工作，不直接改实现角色的文件。", &["plan", "implement"], &["artifacts/coding/verification.md"]),
        ]),
        template("review", "代码审查与缺陷定位", "工程", "并行审查行为正确性与信任边界，再复核为有证据的可行动问题", None, vec![
            stage("correctness", "正确性审查员", "追踪行为和兼容性", "阅读目标变更及调用方，重点检查状态转换、并发、资源释放、边界条件和接口兼容性；每个候选问题提供文件行号、触发输入、实际影响与复现思路。", &[], &["artifacts/review/correctness.md"]),
            stage("boundaries", "边界审查员", "追踪输入与权限边界", "独立追踪外部输入、路径、身份、权限、凭据和输出的流向；检查验证是否发生在正确位置，为候选问题提供实际调用链和触发条件，避免无依据推测。", &[], &["artifacts/review/boundaries.md"]),
            stage("triage", "审查负责人", "复核和去重", "逐项独立验证两个分支的候选问题，运行必要的最小复现，删除误报并合并重复项；输出按影响排序的问题、精确位置、证据和修复建议。无确认问题时明确说明覆盖范围。", &["correctness", "boundaries"], &["artifacts/review/findings.md"]),
        ]),
        template("documents", "文档与方案交付", "办公", "提取材料、撰写正式稿，再校对事实与决策完整性", Some("editor"), vec![
            stage("outline", "资料编辑", "建立事实与结构", "提取受众、目的、事实、约束、术语和决策问题；为每条重要事实标注输入来源，形成文档提纲和缺失输入清单。", &[], &["artifacts/documents/outline.md"]),
            stage("author", "方案作者", "撰写完整初稿", "依据提纲写出正式文档初稿，使用输入提供的真实数字与证据；论证连接问题、方案、取舍和执行步骤，保留来源引用及待填项。", &["outline"], &["artifacts/documents/draft.md"]),
            stage("editor", "文档审核员", "校对后交付正式稿", "核对初稿术语、数字、引文、结构与结论；交付修订后的完整Markdown正式稿，附具体待审核事项。若用户另需PDF或Office格式，明确当前交付格式及尚需转换的工作。", &["outline", "author"], &["artifacts/documents/final.md", "artifacts/documents/review-notes.md"]),
        ]),
        template("automation", "浏览器流程自动化", "自动化", "先观察流程，再执行授权操作，最后核验页面与文件结果", None, vec![
            stage("map", "流程观察员", "记录可执行网页步骤", "使用自己的浏览器上下文观察目标页面，记录URL、所需输入、登录条件、成功标志和错误分支；给出避免重复执行的检查点，不提前完成最终提交。", &[], &["artifacts/automation/procedure.md"]),
            stage("execute", "浏览器执行员", "执行指定流程", "从自己的隔离浏览器重新打开目标页面，按流程完成用户授权操作；执行前后核对当前状态，记录实际步骤、结果标识、最终URL与截图路径。页面可能变化时重新观察，不机械套用旧坐标。", &["map"], &["artifacts/automation/execution.md"]),
            stage("verify", "结果核验员", "独立确认交付", "读取执行记录及截图、输出文件，必要时在自己的浏览器以只读方式检查最终URL或结果标识。各agent不共享登录状态，无法独立访问时说明验证限制；核对成功标志、遗漏项与实际交付，不重复提交。", &["map", "execute"], &["artifacts/automation/verification.md"]),
        ]),
        template("incident", "故障排查与根因分析", "工程", "从证据与假设检验到修复、回归；启动前为修复角色分配实际需改文件", Some("regress"), vec![
            stage("evidence", "故障调查员", "整理时间线与复现", "读取实际日志、环境和用户报告，记录故障时间线、症状、预期行为、影响范围与可重复步骤；保留原始证据路径，区分已观察事实与猜测。", &[], &["artifacts/incident/evidence.md"]),
            stage("diagnose", "根因分析师", "检验竞争假设", "基于证据提出按可能性排序的根因假设，通过最小实验或代码追踪逐项检验；记录支持与否定证据、确认根因、修复范围和回归用例。", &["evidence"], &["artifacts/incident/diagnosis.md"]),
            stage("repair", "修复工程师", "实施最小修复", "依据已验证根因修改本角色已分配的真实文件，运行直接复现，保存patch并记录结果与恢复步骤。若未分配真实源码或配置文件，仅生成修复patch和操作方案，明确尚未实施。", &["diagnose"], &["artifacts/incident/repair.patch", "artifacts/incident/repair.md"]),
            stage("regress", "回归验证员", "验证修复与影响范围", "根据原始复现、诊断和修复记录验证故障是否消失，并检查相邻正常场景；记录实际命令、观察结果、未实施步骤和残余问题，形成根因与恢复报告。", &["evidence", "diagnose", "repair"], &["artifacts/incident/postmortem.md"]),
        ]),
    ]
}
pub fn list(home: &Path) -> Result<Vec<TeamTemplate>> {
    let mut result = builtins();
    let dir = home.join("templates");
    if !dir.exists() {
        return Ok(result);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            result.push(serde_json::from_slice(&fs::read(path)?)?);
        }
    }
    Ok(result)
}
pub fn save(home: &Path, mut template: TeamTemplate) -> Result<TeamTemplate> {
    ensure!(
        !template.name.trim().is_empty(),
        "template name is required"
    );
    ensure!(
        template.agents.len() <= 32 && !template.agents.is_empty(),
        "template needs 1–32 agents"
    );
    crate::harness::validate_graph(&template.agents)?;
    if template.id.is_empty() || template.id.starts_with("builtin-") {
        template.id = Uuid::new_v4().to_string();
    }
    Uuid::parse_str(&template.id)?;
    template.builtin = false;
    for agent in &mut template.agents {
        agent.session_id.clear();
        agent.status = "pending".into();
        agent.output.clear();
        agent.error = None;
    }
    let dir = home.join("templates");
    fs::create_dir_all(&dir)?;
    let temp = dir.join(format!("{}.tmp", Uuid::new_v4()));
    fs::write(&temp, serde_json::to_vec_pretty(&template)?)?;
    fs::rename(temp, dir.join(format!("{}.json", template.id)))?;
    Ok(template)
}
pub fn delete(home: &Path, id: &str) -> Result<()> {
    Uuid::parse_str(id)?;
    fs::remove_file(home.join("templates").join(format!("{id}.json")))?;
    Ok(())
}
impl From<TeamTemplate> for TeamSpec {
    fn from(t: TeamTemplate) -> Self {
        Self {
            name: t.name,
            goal: t.goal,
            agents: t.agents,
            files: t.files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saves_and_reloads_full_template_content() {
        let home = std::env::temp_dir().join(format!("aris-templates-{}", Uuid::new_v4()));
        let mut template = builtins().remove(0);
        let text = "调研任务与数据边界\n".repeat(10_000);
        template.name = "季度调研".repeat(120);
        template.description = text.clone();
        template.goal = text.clone();
        template.agents[0].prompt = text.clone();
        let saved = save(&home, template).unwrap();
        let reloaded = list(&home)
            .unwrap()
            .into_iter()
            .find(|t| t.id == saved.id)
            .unwrap();
        assert_eq!(reloaded.name, saved.name);
        assert_eq!(reloaded.description, text);
        assert_eq!(reloaded.goal, text);
        assert_eq!(reloaded.agents[0].prompt, text);
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn every_builtin_has_valid_dependencies_and_can_be_copied() {
        let home = std::env::temp_dir().join(format!("aris-templates-{}", Uuid::new_v4()));
        for t in builtins() {
            crate::harness::validate_graph(&t.agents).unwrap();
            let saved = save(&home, t).unwrap();
            assert!(!saved.builtin);
        }
        assert_eq!(list(&home).unwrap().len(), 20);
        fs::remove_dir_all(home).unwrap();
    }
}
