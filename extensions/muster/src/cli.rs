//! The `@muster` verbs.
//!
//! Human output is tab-separated; `--json` sends one `result` payload
//! *instead of* the text, never alongside it — in plain mode the CLI writes a
//! RESULT straight to stdout, so sending both prints the answer twice.

use super::{Muster, describe_ready};
use blit_ext_muster::config;
use blit_ext_muster::journal::{Cause, quote};
use blit_ext_muster::supervisor::Phase;
use blit_guest::Client;
use blit_guest::command::Invocation;

/// Emitted by `@muster schema`, so an editor validates a unit file as it is
/// typed. Kept deliberately shallow: it is a completion and typo aid, not a
/// second implementation of the parser.
const SCHEMA: &str = include_str!("schema.json");

impl Muster {
    pub(crate) fn serve(&mut self, client: &mut Client, mut invocation: Invocation) {
        let args = invocation.request().args.clone();
        let json = args.iter().any(|a| a == "--json");
        let values = args.iter().any(|a| a == "--values");
        let positional: Vec<&str> = args
            .iter()
            .map(String::as_str)
            .filter(|a| !a.starts_with('-'))
            .collect();
        let verb = positional.first().copied().unwrap_or("list");
        let target = positional.get(1).copied();

        let (code, text) = match verb {
            "list" => (0, self.render_list(json)),
            "status" => match target {
                Some(name) => self.render_status(name, json),
                None => (2, String::from("status needs a name\n")),
            },
            "start" | "stop" | "restart" => match target {
                Some(name) => self.act(client, verb, name),
                None => (2, format!("{verb} needs a name\n")),
            },
            "ready" => match target {
                Some(name) => self.mark_ready(client, name),
                None => (2, String::from("ready needs a unit\n")),
            },
            "reload" => {
                self.load(client);
                (0, String::from("reloaded\n"))
            }
            "log" => (0, self.render_log(&args, json)),
            "cat" => match target {
                Some(name) => self.render_cat(name),
                None => (2, String::from("cat needs a name\n")),
            },
            "env" => match target {
                Some(name) => self.render_env(client, name, values, json),
                None => (2, String::from("env needs a unit\n")),
            },
            "stacks" => (0, self.render_stacks(json)),
            "doctor" => self.render_doctor(json),
            "schema" => (0, format!("{SCHEMA}\n")),
            other => (2, format!("unknown verb {other:?}\n")),
        };

        let sent = if json && code == 0 && looks_like_json(&text) {
            invocation.result(client, "application/json", text.as_bytes())
        } else {
            invocation.stdout(client, text.as_bytes())
        };
        let _ = sent;
        let _ = invocation.exit(client, code, "");
    }

    // ------------------------------------------------------------------ verbs

    fn act(&mut self, client: &mut Client, verb: &str, name: &str) -> (i32, String) {
        let members = self.resolve(name);
        if members.is_empty() {
            return (1, format!("no unit or instance named {name:?}\n"));
        }
        for member in &members {
            match verb {
                "start" => self.want(client, member, Cause::Command),
                "stop" => self.stop_one(client, member, Cause::Command, true),
                _ => self.restart(client, member, Cause::Command),
            }
        }
        (0, format!("{verb}ed {}\n", members.join(" ")))
    }

    fn mark_ready(&mut self, client: &mut Client, name: &str) -> (i32, String) {
        match self.units.get(name) {
            Some(unit) if unit.phase == Phase::Activating => {
                self.ready(client, name, "manual");
                (0, format!("{name} is ready\n"))
            }
            Some(unit) => (
                1,
                format!("{name} is {}, not activating\n", unit.phase.as_str()),
            ),
            None => (1, format!("no unit named {name:?}\n")),
        }
    }

    /// A name is a unit, or an instance, in which case the verb applies to
    /// every unit in it.
    fn resolve(&self, name: &str) -> Vec<String> {
        if self.units.contains_key(name) {
            return vec![name.to_string()];
        }
        match self.instances.get(name) {
            Some(instance) => instance.members.clone(),
            None => Vec::new(),
        }
    }

    // -------------------------------------------------------------- rendering

    fn render_list(&self, json: bool) -> String {
        if json {
            let units: Vec<String> = self.units.values().map(|u| self.unit_json(u)).collect();
            let instances: Vec<String> = self
                .instances
                .iter()
                .map(|(name, instance)| {
                    let ready = instance
                        .members
                        .iter()
                        .filter(|m| self.units.get(*m).is_some_and(|u| u.phase.is_ready()))
                        .count();
                    format!(
                        r#"{{"name":{},"stack":{},"ready":{ready},"total":{}}}"#,
                        quote(name),
                        quote(&instance.stack),
                        instance.members.len()
                    )
                })
                .collect();
            return format!(
                "{{\"instances\":[{}],\"units\":[{}]}}\n",
                instances.join(","),
                units.join(",")
            );
        }

        let mut out = String::from("NAME\tPHASE\tPTY\tRESTARTS\tDESCRIPTION\n");
        for (name, unit) in &self.units {
            if unit.instance.is_some() {
                continue;
            }
            out.push_str(&self.unit_row(name, unit));
        }
        for (name, instance) in &self.instances {
            let ready = instance
                .members
                .iter()
                .filter(|m| self.units.get(*m).is_some_and(|u| u.phase.is_ready()))
                .count();
            out.push_str(&format!(
                "{name}\t—\t-\t-\t{}, {ready}/{} ready\n",
                instance.stack,
                instance.members.len()
            ));
            for member in &instance.members {
                if let Some(unit) = self.units.get(member) {
                    out.push_str("  ");
                    out.push_str(&self.unit_row(member, unit));
                }
            }
        }
        out
    }

    fn unit_row(&self, name: &str, unit: &blit_ext_muster::supervisor::Unit) -> String {
        format!(
            "{name}\t{}\t{}\t{}\t{}\n",
            unit.phase.as_str(),
            unit.pty.map_or_else(|| "-".into(), |p| p.to_string()),
            unit.failures,
            unit.file.description.clone().unwrap_or_default()
        )
    }

    fn unit_json(&self, unit: &blit_ext_muster::supervisor::Unit) -> String {
        let mut out = format!(
            r#"{{"name":{},"phase":"{}","restarts":{}"#,
            quote(&unit.name),
            unit.phase.as_str(),
            unit.failures
        );
        if let Some(instance) = &unit.instance {
            out.push_str(&format!(r#","instance":{}"#, quote(instance)));
        }
        if let Some(pty) = unit.pty {
            out.push_str(&format!(r#","pty":{pty}"#));
        }
        if let Some(exit) = unit.last_exit {
            out.push_str(&format!(r#","lastExit":{exit}"#));
        }
        if let Some(description) = &unit.file.description {
            out.push_str(&format!(r#","description":{}"#, quote(description)));
        }
        let requires: Vec<String> = unit.file.requires.iter().map(|r| quote(r)).collect();
        out.push_str(&format!(r#","requires":[{}]}}"#, requires.join(",")));
        out
    }

    /// `status` ends with the retained runs — the reason `keep` exists.
    fn render_status(&self, name: &str, json: bool) -> (i32, String) {
        let Some(unit) = self.units.get(name) else {
            return match self.instances.get(name) {
                Some(_) => (0, self.render_list(json)),
                None => (1, format!("no unit named {name:?}\n")),
            };
        };
        if json {
            let runs: Vec<String> = unit
                .runs
                .iter()
                .map(|r| {
                    format!(
                        r#"{{"pty":{},"seq":{},"exitCode":{},"endedMs":{}}}"#,
                        r.pty, r.seq, r.exit_code, r.ended_ms
                    )
                })
                .collect();
            let mut out = self.unit_json(unit);
            out.pop();
            return (0, format!("{out},\"runs\":[{}]}}\n", runs.join(",")));
        }
        let mut out = String::new();
        out.push_str(&format!("unit\t{name}\n"));
        out.push_str(&format!("phase\t{}\n", unit.phase.as_str()));
        if let Some(instance) = &unit.instance {
            out.push_str(&format!("instance\t{instance}\n"));
        }
        out.push_str(&format!(
            "ready-when\t{}\n",
            describe_ready(&unit.file.ready_when)
        ));
        if let Some(pty) = unit.pty {
            out.push_str(&format!("pty\t{pty}\n"));
        }
        out.push_str(&format!("failures\t{}\n", unit.failures));
        if let Some(exit) = unit.last_exit {
            out.push_str(&format!("last-exit\t{exit}\n"));
        }
        if unit.stale {
            out.push_str("stale\tthe file changed since this run started\n");
        }
        for run in &unit.runs {
            out.push_str(&format!(
                "run\t{}\texit {}\tseq {}\n",
                run.pty, run.exit_code, run.seq
            ));
        }
        (0, out)
    }

    fn render_log(&self, args: &[String], json: bool) -> String {
        let value_of = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|at| args.get(at + 1))
                .cloned()
        };
        let count: usize = value_of("-n").and_then(|n| n.parse().ok()).unwrap_or(50);
        let unit_filter = value_of("-u");
        let since: Option<u64> = value_of("--since").and_then(|s| s.parse().ok());

        let matches = |record: &blit_ext_muster::journal::Record| {
            unit_filter.as_ref().is_none_or(|want| {
                let want = want.trim_start_matches('@');
                record.unit == *want || record.instance.as_deref() == Some(want)
            })
        };
        let selected: Vec<&blit_ext_muster::journal::Record> = match since {
            Some(seq) => self.journal.since(seq).filter(|r| matches(r)).collect(),
            None => {
                let all: Vec<_> = self
                    .journal
                    .tail(usize::MAX)
                    .filter(|r| matches(r))
                    .collect();
                all.into_iter().rev().take(count).rev().collect()
            }
        };
        if json {
            let records: Vec<String> = selected.iter().map(|r| r.to_json()).collect();
            return format!("[{}]\n", records.join(","));
        }
        let mut out = String::new();
        for record in selected {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                record.seq,
                record.unit,
                record.event,
                record.phase,
                record
                    .cause
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| record.detail.clone())
            ));
        }
        out
    }

    /// The file behind a name, wherever it is watched from.
    ///
    /// A plain unit is `<name>.json` in the configuration directory, or in an
    /// included one. A stack member is `<template>.json` under the stack, which
    /// may be a subdirectory or a directory anywhere.
    fn render_cat(&self, name: &str) -> (i32, String) {
        let mut candidates: Vec<(String, String)> =
            vec![(self.dir.clone(), format!("{name}.json"))];
        if let Some((template, instance)) = name.rsplit_once('@')
            && let Some(instance) = self.instances.get(instance)
        {
            let stack_dir = self.resolve_path(&instance.stack);
            if config::is_path(&instance.stack) {
                candidates.push((stack_dir, format!("{template}.json")));
            } else {
                candidates.push((
                    self.dir.clone(),
                    format!("{}/{template}.json", instance.stack),
                ));
            }
        }
        for root in &self.roots {
            candidates.push((root.path.clone(), format!("{name}.json")));
        }
        for (root, relative) in candidates {
            if let Some(content) = self.file_at(&root, &relative) {
                return (0, String::from_utf8_lossy(&content).into_owned());
            }
        }
        (1, format!("no file behind {name:?}\n"))
    }

    fn render_env(
        &mut self,
        client: &mut Client,
        name: &str,
        values: bool,
        json: bool,
    ) -> (i32, String) {
        let Some(unit) = self.units.get(name) else {
            return (1, format!("no unit named {name:?}\n"));
        };
        let home = self.home();
        let cwd = super::expand_tilde(unit.file.cwd.as_deref().unwrap_or("~"), &home);
        let (env, _, failure) = self.resolve_env(client, name, &cwd);
        if let Some(failure) = failure {
            return (1, format!("{failure}\n"));
        }
        if json {
            let entries: Vec<String> = env
                .iter()
                .map(|(key, value, origin)| {
                    if values {
                        format!(
                            r#"{{"key":{},"from":{},"value":{}}}"#,
                            quote(key),
                            quote(origin.label()),
                            quote(value)
                        )
                    } else {
                        format!(
                            r#"{{"key":{},"from":{}}}"#,
                            quote(key),
                            quote(origin.label())
                        )
                    }
                })
                .collect();
            return (0, format!("[{}]\n", entries.join(",")));
        }
        let mut out = String::new();
        for (key, value, origin) in &env {
            if values {
                out.push_str(&format!("{key}\t{}\t{value}\n", origin.label()));
            } else {
                out.push_str(&format!("{key}\t{}\n", origin.label()));
            }
        }
        (0, out)
    }

    fn render_stacks(&self, json: bool) -> String {
        if json {
            let stacks: Vec<String> = self
                .stacks
                .iter()
                .map(|(name, stack)| {
                    let vars: Vec<String> = stack
                        .vars
                        .iter()
                        .map(|(var, decl)| {
                            format!(
                                r#"{{"name":{},"required":{},"kind":{}}}"#,
                                quote(var),
                                decl.required,
                                decl.kind.as_deref().map_or("null".into(), quote)
                            )
                        })
                        .collect();
                    format!(r#"{{"name":{},"vars":[{}]}}"#, quote(name), vars.join(","))
                })
                .collect();
            return format!("[{}]\n", stacks.join(","));
        }
        let mut out = String::from("STACK\tPARAMETER\tREQUIRED\tKIND\n");
        for (name, stack) in &self.stacks {
            if stack.vars.is_empty() {
                out.push_str(&format!("{name}\t-\t-\t-\n"));
            }
            for (var, decl) in &stack.vars {
                out.push_str(&format!(
                    "{name}\t{var}\t{}\t{}\n",
                    decl.required,
                    decl.kind.as_deref().unwrap_or("-")
                ));
            }
        }
        out
    }

    /// Everything wrong with the directory, in one pass.
    fn render_doctor(&self, json: bool) -> (i32, String) {
        let mut findings: Vec<(String, String)> = self
            .findings
            .iter()
            .map(|f| (f.file.clone(), f.detail.clone()))
            .collect();

        for (name, unit) in &self.units {
            for key in unit.file.unknown_keys() {
                findings.push((name.clone(), format!("unknown key {key:?}")));
            }
            for dep in unit
                .file
                .requires
                .iter()
                .chain(&unit.file.wants)
                .chain(&unit.file.after)
            {
                if !self.units.contains_key(dep) {
                    findings.push((
                        name.clone(),
                        format!("depends on {dep:?}, which does not exist"),
                    ));
                }
            }
        }
        let roots: Vec<String> = self.units.keys().cloned().collect();
        if let Err(blit_ext_muster::supervisor::Cycle(ring)) =
            blit_ext_muster::supervisor::start_order(&self.units, &roots)
        {
            findings.push((ring.join(" -> "), String::from("dependency cycle")));
        }

        if json {
            let items: Vec<String> = findings
                .iter()
                .map(|(where_, what)| {
                    format!(r#"{{"where":{},"what":{}}}"#, quote(where_), quote(what))
                })
                .collect();
            return (
                i32::from(!findings.is_empty()),
                format!("[{}]\n", items.join(",")),
            );
        }
        if findings.is_empty() {
            return (0, String::from("no findings\n"));
        }
        let mut out = String::new();
        for (where_, what) in &findings {
            out.push_str(&format!("{where_}\t{what}\n"));
        }
        (1, out)
    }
}

fn looks_like_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}
