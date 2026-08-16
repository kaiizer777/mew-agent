use mew_agent::chat_agent::ChatAgent;
use mew_agent::handoff::JobQuery;
use mew_agent::research::ResearchPlatforms;
use mew_agent::router::Intent;
use std::fs;

#[tokio::main]
async fn main() {
    let config_str = fs::read_to_string("config.yaml")
        .or_else(|_| fs::read_to_string("../config.yaml"))
        .unwrap();
    let config: mew_agent::ProviderConfig = serde_yaml::from_str(&config_str).unwrap();

    let platforms = ResearchPlatforms::load_from_default_location().to_research_subtasks();

    let chat_agent = ChatAgent::new(config.clone());

    let intent = Intent::BrowserTask {
        task: "Find me a Rust software engineering internship in San Francisco that allows remote."
            .to_string(),
        shape: mew_agent::router::TaskShape::JobSearch,
        job_query: Some(JobQuery {
            roles: vec!["Rust Software Engineer".to_string()],
            locations: vec!["San Francisco".to_string()],
            is_remote: true,
            is_internship: true,
            experience_level: None,
        }),
        lead_query: None,
        confidence: None,
        is_ambiguous: false,
    };

    if let Intent::BrowserTask {
        task,
        shape: _,
        job_query,
        lead_query,
        ..
    } = intent
    {
        let mut handoff = chat_agent.build_handoff_with_platforms(
            &task,
            "msg-123",
            vec![],
            &platforms,
            job_query.clone(),
            lead_query.clone(),
        );
        handoff.job_query = job_query;

        println!("Handoff Subtasks (if fallback):");
        for subtask in &handoff.subtasks {
            println!("  - {}", subtask.description);
        }

        if let Some(plan) = &handoff.research_plan {
            println!("\nResearch Plan Platforms:");
            for p in &plan.platforms {
                println!("  Platform: {}", p.platform);
                println!("  Query:    {}", p.query);
                println!("  Hint:     {}", p.entry_hint);
                println!("  Accept:   {:?}", p.acceptance);
                println!();
            }
        }
    }
}
