//! `mimic rules` (alias `dict`): your dictionary of words and "heard X → write Y" rules.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use mimic_core::config::Config;
use mimic_core::ipc::Client;
use mimic_core::rules::{RuleSet, SearchHit};
use serde_json::json;

use crate::ui::{bold, dim, paint, print_json};

#[derive(Subcommand)]
pub enum RulesCmd {
    /// List rules, optionally filtered by a search.
    #[command(visible_alias = "ls")]
    List { query: Option<String> },
    /// Add a word or a replacement. Without --heard it is a vocabulary word (helps
    /// recognition and fixes its casing).
    Add(AddRule),
    /// Remove a whole rule (by what it writes).
    #[command(visible_alias = "rm")]
    Remove { target: String },
    /// Remove one "heard" variant from a rule.
    RemoveHeard { target: String, heard: String },
    /// Preview how a sentence would come out with your rules and text settings.
    Test { text: String },
    /// Import rules from a JSON file (as written by `export`).
    Import { file: std::path::PathBuf },
    /// Export all rules as JSON (to a file or stdout).
    Export { file: Option<std::path::PathBuf> },
}

#[derive(Args)]
pub struct AddRule {
    /// What should be written (e.g. "Omarchy").
    pub target: String,
    /// What the model writes instead (repeatable): --heard "on march 9".
    #[arg(long = "heard", short = 'H')]
    pub heard: Vec<String>,
}

pub fn run(r: RulesCmd, json_out: bool) -> Result<()> {
    let client = Client::connect().ok();
    // Without the daemon, edit the file directly.
    let local = || -> Result<RuleSet> { RuleSet::load() };
    match r {
        RulesCmd::List { query } => {
            let hits: Vec<SearchHit> = match client {
                Some(mut c) => serde_json::from_value(c.call("rules.list", json!({"query": query.clone().unwrap_or_default()}))?)?,
                None => local()?.search(query.as_deref().unwrap_or(""), 500),
            };
            if json_out {
                print_json(&serde_json::to_value(&hits)?);
                return Ok(());
            }
            if hits.is_empty() {
                println!("{}", dim("No rules yet. Add one: mimic rules add Omarchy --heard \"on march 9\" — or press Ctrl+Win+R right after dictating."));
            }
            for h in hits {
                let heard = if h.rule.variants.is_empty() { dim("(vocabulary word)") } else { h.rule.variants.join(dim(", ").as_str()) };
                let uses = if h.rule.uses > 0 { dim(&format!("  ·{}×", h.rule.uses)) } else { String::new() };
                println!("  {:<24} {} {}{}", bold(&h.rule.target), dim("←"), heard, uses);
            }
            Ok(())
        }
        RulesCmd::Add(a) => {
            let out = match client {
                Some(mut c) => c.call("rules.upsert", json!({"target": a.target, "variants": a.heard}))?,
                None => {
                    let mut rs = local()?;
                    let o = rs.upsert(&a.target, &a.heard);
                    rs.save()?;
                    serde_json::to_value(o)?
                }
            };
            if json_out {
                print_json(&out);
                return Ok(());
            }
            let rule = &out["rule"];
            println!(
                "{} {} {} {}",
                if out["created"].as_bool() == Some(true) { paint("added", "32") } else { paint("updated", "36") },
                bold(rule["target"].as_str().unwrap_or("")),
                dim("←"),
                rule["variants"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
            );
            for m in out["moved"].as_array().cloned().unwrap_or_default() {
                println!("  {}", dim(&format!("moved \"{}\" here from \"{}\"", m[0].as_str().unwrap_or(""), m[1].as_str().unwrap_or(""))));
            }
            Ok(())
        }
        RulesCmd::Remove { target } => {
            let ok = match client {
                Some(mut c) => c.call("rules.delete", json!({"target": target}))?.as_bool().unwrap_or(false),
                None => {
                    let mut rs = local()?;
                    let ok = rs.remove(&target);
                    rs.save()?;
                    ok
                }
            };
            if !ok {
                bail!("no rule writes \"{target}\"");
            }
            println!("removed {target}");
            Ok(())
        }
        RulesCmd::RemoveHeard { target, heard } => {
            let ok = match client {
                Some(mut c) => c.call("rules.delete_variant", json!({"target": target, "variant": heard}))?.as_bool().unwrap_or(false),
                None => {
                    let mut rs = local()?;
                    let ok = rs.remove_variant(&target, &heard);
                    rs.save()?;
                    ok
                }
            };
            if !ok {
                bail!("\"{target}\" has no variant \"{heard}\"");
            }
            println!("removed \"{heard}\" from {target}");
            Ok(())
        }
        RulesCmd::Test { text } => {
            let out = match client {
                Some(mut c) => c.call("rules.preview", json!({"text": text}))?,
                None => {
                    let rs = local()?;
                    let cfg = Config::load()?;
                    let tr = mimic_core::text::Transcript::from_text(&text, 1.0);
                    let p = mimic_core::text::process(
                        &tr.words,
                        &cfg.text,
                        Some(&rs.matcher()),
                        mimic_core::text::Context { sentence_start: true, is_final: true },
                    );
                    json!({"text": p.text, "rules": p.rules_hit})
                }
            };
            if json_out {
                print_json(&out);
            } else {
                println!("{}", out["text"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        RulesCmd::Import { file } => {
            let set: RuleSet = serde_json::from_str(&std::fs::read_to_string(&file)?).context("not a Mimic rules file")?;
            let n = set.rules.len();
            match client {
                Some(mut c) => {
                    c.call("rules.import", json!({"rules": set}))?;
                }
                None => {
                    let mut rs = local()?;
                    for r in set.rules {
                        rs.upsert(&r.target, &r.variants);
                    }
                    rs.save()?;
                }
            }
            println!("imported {n} rules");
            Ok(())
        }
        RulesCmd::Export { file } => {
            let rs = local()?;
            let s = serde_json::to_string_pretty(&rs)?;
            match file {
                Some(f) => {
                    std::fs::write(&f, s)?;
                    println!("exported {} rules to {}", rs.rules.len(), f.display());
                }
                None => println!("{s}"),
            }
            Ok(())
        }
    }
}
