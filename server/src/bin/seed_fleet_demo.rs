use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct DemoSkill {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    tags: &'static [&'static str],
    examples: [&'static str; 2],
}

struct DemoAgent {
    name: &'static str,
    display_name: &'static str,
    description: &'static str,
    version: &'static str,
    status: &'static str,
    tags: &'static [&'static str],
    skills: &'static [DemoSkill],
    updated_days_ago: i64,
}

const fn skill(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    tags: &'static [&'static str],
    examples: [&'static str; 2],
) -> DemoSkill {
    DemoSkill {
        id,
        name,
        description,
        tags,
        examples,
    }
}

const AGENTS: &[DemoAgent] = &[
    DemoAgent {
        name: "demo-support-triage",
        display_name: "Support Triage",
        description: "Reads incoming support tickets, labels urgency and product area, and routes each one to the right queue.",
        version: "2.3.1",
        status: "running",
        tags: &["support", "routing", "classification"],
        skills: &[
            skill(
                "triage-ticket",
                "Triage a ticket",
                "Labels urgency and product area for one ticket.",
                &["support"],
                [
                    "Triage this ticket: the export button does nothing",
                    "How urgent is a login outage for one customer?",
                ],
            ),
            skill(
                "route-ticket",
                "Route to a queue",
                "Chooses the support queue that owns the ticket.",
                &["routing"],
                [
                    "Which queue handles billing disputes?",
                    "Route this refund request",
                ],
            ),
        ],
        updated_days_ago: 1,
    },
    DemoAgent {
        name: "demo-support-escalation",
        display_name: "Support Escalation",
        description: "Watches open tickets for breached response times and drafts escalation notes for the on-call lead.",
        version: "1.8.0",
        status: "running",
        tags: &["support", "sla", "alerts"],
        skills: &[skill(
            "draft-escalation",
            "Draft an escalation",
            "Writes a short escalation note with the ticket history.",
            &["support"],
            [
                "Escalate ticket 4821 to the on-call lead",
                "Which tickets breached their SLA today?",
            ],
        )],
        updated_days_ago: 3,
    },
    DemoAgent {
        name: "demo-code-review",
        display_name: "Code Review",
        description: "Reviews pull requests for bugs, risky changes and missing tests, and leaves line-level comments.",
        version: "3.1.0",
        status: "running",
        tags: &["code", "github", "quality", "review"],
        skills: &[
            skill(
                "review-pr",
                "Review a pull request",
                "Finds bugs and risky changes in a diff.",
                &["code"],
                [
                    "Review PR 1290 in the payments repo",
                    "Is this migration safe to merge?",
                ],
            ),
            skill(
                "suggest-tests",
                "Suggest tests",
                "Lists the tests a change is missing.",
                &["quality"],
                [
                    "What tests does this change need?",
                    "Which edge cases are untested here?",
                ],
            ),
        ],
        updated_days_ago: 2,
    },
    DemoAgent {
        name: "demo-code-refactor",
        display_name: "Code Refactor",
        description: "Proposes small, behaviour-preserving refactors for long functions and duplicated code.",
        version: "0.9.4",
        status: "deploying",
        tags: &["code", "refactoring"],
        skills: &[skill(
            "refactor-function",
            "Refactor a function",
            "Splits a long function into smaller ones without changing behaviour.",
            &["code"],
            [
                "Refactor parse_invoice into smaller functions",
                "Remove the duplication in these two handlers",
            ],
        )],
        updated_days_ago: 0,
    },
    DemoAgent {
        name: "demo-pr-summarizer",
        display_name: "PR Summarizer",
        description: "Turns a pull request into a three-line summary for reviewers and release managers.",
        version: "1.2.0",
        status: "running",
        tags: &["code", "github", "summaries"],
        skills: &[skill(
            "summarize-pr",
            "Summarize a pull request",
            "Writes a three-line summary of a diff.",
            &["summaries"],
            ["Summarize PR 877", "What changed in the last merged PR?"],
        )],
        updated_days_ago: 6,
    },
    DemoAgent {
        name: "demo-finops-analyst",
        display_name: "FinOps Analyst",
        description: "Explains AI spend changes week over week and points to the agents and models behind them.",
        version: "2.0.2",
        status: "running",
        tags: &["finance", "analytics", "cost"],
        skills: &[
            skill(
                "explain-spend",
                "Explain a spend change",
                "Breaks a spend change down by agent and model.",
                &["finance"],
                [
                    "Why did spend go up last week?",
                    "Which model costs us the most?",
                ],
            ),
            skill(
                "forecast-spend",
                "Forecast spend",
                "Projects next month's spend from the recent trend.",
                &["finance", "forecasting"],
                [
                    "What will we spend next month?",
                    "Are we on track for the quarter?",
                ],
            ),
        ],
        updated_days_ago: 4,
    },
    DemoAgent {
        name: "demo-invoice-classifier",
        display_name: "Invoice Classifier",
        description: "Classifies supplier invoices by cost centre and flags duplicates before they reach accounts payable.",
        version: "1.5.3",
        status: "failed",
        tags: &["finance", "documents", "classification"],
        skills: &[skill(
            "classify-invoice",
            "Classify an invoice",
            "Assigns a cost centre and flags duplicates.",
            &["finance"],
            [
                "Classify this invoice from Acme Corp",
                "Is invoice 2231 a duplicate?",
            ],
        )],
        updated_days_ago: 1,
    },
    DemoAgent {
        name: "demo-rag-search",
        display_name: "Knowledge Search",
        description: "Answers questions from the internal knowledge base and cites the documents it used.",
        version: "4.2.0",
        status: "running",
        tags: &["search", "docs", "rag"],
        skills: &[
            skill(
                "answer-question",
                "Answer a question",
                "Answers from the knowledge base with citations.",
                &["search"],
                [
                    "How do I rotate an API key?",
                    "What is our data retention policy?",
                ],
            ),
            skill(
                "find-document",
                "Find a document",
                "Finds the document that covers a topic.",
                &["docs"],
                [
                    "Find the onboarding checklist",
                    "Where is the incident runbook?",
                ],
            ),
        ],
        updated_days_ago: 2,
    },
    DemoAgent {
        name: "demo-doc-qa",
        display_name: "Doc Q&A",
        description: "Answers questions about one uploaded document, such as a contract or a design spec.",
        version: "1.1.0",
        status: "stopped",
        tags: &["docs", "contracts"],
        skills: &[skill(
            "ask-document",
            "Ask about a document",
            "Answers questions about a single uploaded file.",
            &["docs"],
            [
                "What is the notice period in this contract?",
                "Summarize section 3 of the spec",
            ],
        )],
        updated_days_ago: 12,
    },
    DemoAgent {
        name: "demo-image-caption",
        display_name: "Image Captioner",
        description: "Writes alt text and short captions for product images in the content library.",
        version: "0.6.1",
        status: "registered",
        tags: &["content", "images", "accessibility"],
        skills: &[skill(
            "caption-image",
            "Caption an image",
            "Writes alt text and a caption for one image.",
            &["content"],
            [
                "Write alt text for this product photo",
                "Caption the hero image",
            ],
        )],
        updated_days_ago: 20,
    },
    DemoAgent {
        name: "demo-lead-classifier",
        display_name: "Lead Classifier",
        description: "Scores new sales leads by fit and intent and assigns them to the right sales region.",
        version: "2.4.0",
        status: "running",
        tags: &["sales", "crm", "classification", "scoring"],
        skills: &[skill(
            "score-lead",
            "Score a lead",
            "Scores fit and intent for one lead.",
            &["sales"],
            [
                "Score the lead from Globex",
                "Which new leads are worth a call today?",
            ],
        )],
        updated_days_ago: 1,
    },
    DemoAgent {
        name: "demo-email-router",
        display_name: "Email Router",
        description: "Routes shared-inbox emails to the team that owns them and drafts a first reply.",
        version: "1.9.2",
        status: "running",
        tags: &["email", "routing", "support"],
        skills: &[skill(
            "route-email",
            "Route an email",
            "Picks the owning team and drafts a reply.",
            &["email"],
            [
                "Who should answer this partnership email?",
                "Draft a reply to this billing question",
            ],
        )],
        updated_days_ago: 5,
    },
    DemoAgent {
        name: "demo-crm-enrichment",
        display_name: "CRM Enrichment",
        description: "Fills in missing company size, industry and location on CRM records from public sources.",
        version: "0.8.0",
        status: "stopped",
        tags: &["sales", "crm", "data"],
        skills: &[skill(
            "enrich-record",
            "Enrich a record",
            "Adds company size, industry and location.",
            &["crm"],
            [
                "Enrich the Initech account",
                "Which accounts are missing an industry?",
            ],
        )],
        updated_days_ago: 15,
    },
    DemoAgent {
        name: "demo-sql-generator",
        display_name: "SQL Generator",
        description: "Turns a plain-language question into a read-only SQL query against the analytics warehouse.",
        version: "3.0.1",
        status: "running",
        tags: &["data", "sql", "analytics"],
        skills: &[
            skill(
                "write-query",
                "Write a query",
                "Writes a read-only SQL query for a question.",
                &["sql"],
                [
                    "How many orders shipped last week?",
                    "Top 10 customers by revenue this year",
                ],
            ),
            skill(
                "explain-query",
                "Explain a query",
                "Explains what an SQL query does, step by step.",
                &["sql"],
                ["Explain this query", "Why is this query slow?"],
            ),
        ],
        updated_days_ago: 3,
    },
    DemoAgent {
        name: "demo-onboarding-guide",
        display_name: "Onboarding Guide",
        description: "Walks new employees through their first-week checklist and answers setup questions.",
        version: "1.3.0",
        status: "registered",
        tags: &["hr", "onboarding"],
        skills: &[skill(
            "first-week",
            "Plan the first week",
            "Builds a first-week plan for a new hire.",
            &["onboarding"],
            [
                "What should I do on my first day?",
                "How do I get VPN access?",
            ],
        )],
        updated_days_ago: 25,
    },
    DemoAgent {
        name: "demo-release-notes",
        display_name: "Release Notes",
        description: "Writes customer-facing release notes from the merged pull requests of a release.",
        version: "2.1.0",
        status: "running",
        tags: &["code", "docs", "releases"],
        skills: &[skill(
            "write-notes",
            "Write release notes",
            "Drafts release notes from merged changes.",
            &["releases"],
            [
                "Write the notes for release 5.2",
                "What changed for customers this sprint?",
            ],
        )],
        updated_days_ago: 7,
    },
    DemoAgent {
        name: "demo-incident-summarizer",
        display_name: "Incident Summarizer",
        description: "Summarizes an incident channel into a timeline, impact and follow-up actions for the postmortem.",
        version: "1.6.4",
        status: "crashed",
        tags: &["ops", "incidents", "summaries"],
        skills: &[skill(
            "summarize-incident",
            "Summarize an incident",
            "Builds a timeline and impact summary.",
            &["incidents"],
            [
                "Summarize yesterday's outage",
                "List the follow-ups from incident 311",
            ],
        )],
        updated_days_ago: 0,
    },
    DemoAgent {
        name: "demo-fast-classifier",
        display_name: "Fast Classifier",
        description: "Low-latency text classifier for spam, language and topic, used by other agents.",
        version: "5.0.0",
        status: "running",
        tags: &["classification", "platform"],
        skills: &[skill(
            "classify-text",
            "Classify text",
            "Labels spam, language and topic.",
            &["classification"],
            ["Is this message spam?", "What language is this text in?"],
        )],
        updated_days_ago: 2,
    },
    DemoAgent {
        name: "demo-chatops-bot",
        display_name: "ChatOps Bot",
        description: "Runs read-only status checks from chat and reports service health back to the channel.",
        version: "1.0.3",
        status: "deploying",
        tags: &["ops", "chat", "monitoring"],
        skills: &[skill(
            "check-service",
            "Check a service",
            "Reports the health of one service.",
            &["ops"],
            [
                "Is the billing service healthy?",
                "Show the status of all APIs",
            ],
        )],
        updated_days_ago: 0,
    },
    DemoAgent {
        name: "demo-cost-optimizer",
        display_name: "Cost Optimizer",
        description: "Finds agents that could use a cheaper model without losing quality and estimates the saving.",
        version: "0.4.2",
        status: "registered",
        tags: &["finance", "cost", "models"],
        skills: &[skill(
            "suggest-model",
            "Suggest a cheaper model",
            "Estimates the saving of switching models.",
            &["cost"],
            [
                "Which agents could use a cheaper model?",
                "How much would switching to a mini model save?",
            ],
        )],
        updated_days_ago: 18,
    },
    DemoAgent {
        name: "demo-deep-reasoner",
        display_name: "Deep Reasoner",
        description: "Works through multi-step planning and analysis questions that need longer reasoning.",
        version: "1.0.0",
        status: "running",
        tags: &["reasoning", "planning"],
        skills: &[skill(
            "plan-steps",
            "Plan the steps",
            "Breaks a goal into an ordered plan.",
            &["planning"],
            [
                "Plan a migration from MySQL to Postgres",
                "What are the risks of this rollout plan?",
            ],
        )],
        updated_days_ago: 9,
    },
    DemoAgent {
        name: "demo-log-triage",
        display_name: "Log Triage",
        description: "Groups error logs into known issues and highlights new error patterns after a deploy.",
        version: "2.2.5",
        status: "failed",
        tags: &["ops", "logs", "monitoring", "incidents", "alerts"],
        skills: &[skill(
            "group-errors",
            "Group errors",
            "Groups error lines into known issues.",
            &["logs"],
            [
                "What new errors appeared after the deploy?",
                "Group today's errors by cause",
            ],
        )],
        updated_days_ago: 1,
    },
    DemoAgent {
        name: "demo-migration-planner",
        display_name: "Migration Planner",
        description: "Plans database and service migrations with rollback steps and a risk list.",
        version: "0.7.0",
        status: "stopped",
        tags: &["ops", "planning", "databases"],
        skills: &[skill(
            "plan-migration",
            "Plan a migration",
            "Writes a migration plan with rollback steps.",
            &["planning"],
            [
                "Plan the move of the orders table",
                "What is the rollback plan for this migration?",
            ],
        )],
        updated_days_ago: 11,
    },
    DemoAgent {
        name: "demo-forecast-analyst",
        display_name: "Forecast Analyst",
        description: "Forecasts demand and usage from historical data and explains the drivers behind the forecast.",
        version: "1.4.1",
        status: "registered",
        tags: &["analytics", "forecasting", "data"],
        skills: &[skill(
            "forecast-demand",
            "Forecast demand",
            "Projects demand from historical usage.",
            &["forecasting"],
            [
                "Forecast next quarter's API traffic",
                "What drives the weekend dip?",
            ],
        )],
        updated_days_ago: 14,
    },
];

fn skills_json(skills: &[DemoSkill]) -> Value {
    Value::Array(
        skills
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "description": s.description,
                    "tags": s.tags,
                    "examples": s.examples,
                })
            })
            .collect(),
    )
}

async fn seed_agent(db: &PgPool, owner_id: Uuid, agent: &DemoAgent) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO agents (name, display_name, owner_id, status) \
         VALUES ($1, $1, $2, 'registered') \
         ON CONFLICT (owner_id, name) WHERE deleted_at IS NULL DO NOTHING",
    )
    .bind(agent.name)
    .bind(owner_id)
    .execute(db)
    .await?;

    let tags: Vec<String> = agent.tags.iter().map(|t| t.to_string()).collect();
    sqlx::query(
        "UPDATE agents SET display_name = $2, description = $3, version = $4, status = $5, \
         tags = $6, skills = $7, image = NULL, updated_at = $8 \
         WHERE name = $1 AND deleted_at IS NULL",
    )
    .bind(agent.name)
    .bind(agent.display_name)
    .bind(agent.description)
    .bind(agent.version)
    .bind(agent.status)
    .bind(&tags)
    .bind(skills_json(agent.skills))
    .bind(Utc::now() - Duration::days(agent.updated_days_ago))
    .execute(db)
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set (same as the main server binary)");
    let db = PgPool::connect(&database_url).await?;

    let owner_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM users WHERE is_superuser = true AND deleted_at IS NULL \
         ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(&db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("no superuser found — create a user before seeding"))?;

    for agent in AGENTS {
        seed_agent(&db, owner_id, agent).await?;
    }

    println!(
        "Seeded catalog details for {} demo agents owned by {owner_id}.",
        AGENTS.len()
    );
    Ok(())
}
