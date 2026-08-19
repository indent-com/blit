//! The `@muster` verbs.
//!
//! Human output is tab-separated; `--json` sends one `result` payload
//! *instead of* the text, never alongside it — in plain mode the CLI writes a
//! RESULT straight to stdout, so sending both prints the answer twice.

use super::{Muster, describe_ready};
use blit_ext_muster::config::{self, InstanceFile};
use blit_ext_muster::journal::{Cause, Record};
use blit_ext_muster::supervisor::{self, Phase, Unit};
use blit_guest::Client;
use blit_guest::command::Invocation;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Emitted by `@muster schema`, so an editor validates a unit file as it is
/// typed. Kept deliberately shallow: it is a completion and typo aid, not a
/// second implementation of the parser.
const SCHEMA: &str = include_str!("schema.json");

/// What `@muster instantiate` was asked for. A struct because it is six
/// things, and six positional arguments is a call nobody can read.
struct Instantiate<'a> {
    stack: &'a str,
    name: &'a str,
    /// `NAME=VALUE`, as typed.
    assignments: &'a [&'a str],
    /// Replace an instance that already exists.
    force: bool,
    /// What goes in the file. False writes `"autostart": false`.
    autostart: bool,
    json: bool,
}

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

        // Whether the answer is JSON is something each verb knows; sniffing the
        // text for a leading brace gets `doctor --json` wrong, since it exits 1
        // when it finds anything.
        let mut structured = false;
        let (code, text) = match verb {
            "list" => {
                structured = json;
                (0, self.render_list(json))
            }
            "status" => match target {
                Some(name) => {
                    structured = json;
                    self.render_status(name, json)
                }
                None => (2, String::from("status needs a name\n")),
            },
            "start" | "stop" | "restart" => match target {
                Some(name) => self.act(client, verb, name),
                None => (2, format!("{verb} needs a name\n")),
            },
            "instantiate" => match (target, positional.get(2).copied()) {
                (Some(stack), Some(name)) => {
                    let answered = self.instantiate(
                        client,
                        &Instantiate {
                            stack,
                            name,
                            assignments: &positional[3..],
                            force: args.iter().any(|a| a == "--force"),
                            autostart: !args.iter().any(|a| a == "--no-start"),
                            json,
                        },
                    );
                    structured = json && answered.0 == 0;
                    answered
                }
                _ => (2, String::from("instantiate needs a stack and a name\n")),
            },
            "ready" => match target {
                Some(name) => self.mark_ready(client, name),
                None => (2, String::from("ready needs a unit\n")),
            },
            // `reload NAME` asks that unit to re-read its own configuration.
            //
            // Bare `reload` does *not* re-read the directory to find your
            // edits — the watch already did, before you could type this. What
            // it does is retry the directories whose watch was refused, which
            // is the one failure the watch cannot report its way out of: there
            // is nothing watching a directory that is not being watched. That
            // retry also happens on its own, on a climbing timer; this is for
            // when you have just created the directory and would rather not
            // wait for it.
            "reload" => match target {
                Some(name) => self.reload_unit(client, name),
                None => self.retry_watches(client),
            },
            "log" => {
                structured = json;
                (0, self.render_log(&args, json))
            }
            "cat" => match target {
                Some(name) => self.render_cat(name),
                None => (2, String::from("cat needs a name\n")),
            },
            "env" => match target {
                Some(name) => {
                    let answered = self.render_env(client, name, values, json);
                    structured = json && answered.0 == 0;
                    answered
                }
                None => (2, String::from("env needs a unit\n")),
            },
            "stacks" => {
                structured = json;
                (0, self.render_stacks(json))
            }
            "doctor" => {
                structured = json;
                self.render_doctor(json)
            }
            "schema" => (0, format!("{SCHEMA}\n")),
            other => (2, format!("unknown verb {other:?}\n")),
        };

        let _ = if structured {
            invocation.result(client, "application/json", text.as_bytes())
        } else {
            invocation.stdout(client, text.as_bytes())
        };
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
                "stop" => self.stop(client, member, Cause::Command, true),
                _ => self.restart(client, member, Cause::Command),
            }
        }
        // Not `{verb}ed`: that spells "stoped".
        let past = match verb {
            "start" => "started",
            "stop" => "stopped",
            _ => "restarted",
        };
        (0, format!("{past} {}\n", members.join(" ")))
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

    /// Retry the directories whose watch was refused, now.
    ///
    /// The answer says how many there were, because "nothing was broken" and
    /// "I retried four things" are different outcomes and the old bare `reload`
    /// reported "reloaded" for both — which read as though it had done
    /// something about an edit it had nothing to do with.
    fn retry_watches(&mut self, client: &mut Client) -> (i32, String) {
        let stuck: Vec<String> = self.unwatchable.keys().cloned().collect();
        if stuck.is_empty() {
            return (
                0,
                String::from("every directory is watched; your edits are already in\n"),
            );
        }
        let now = self.now_ms(client);
        self.retry_unwatchable(client, now, true);
        let mut out = String::new();
        for path in &stuck {
            let state = if self.unwatchable.contains_key(path) {
                "still refused"
            } else {
                "watched"
            };
            out.push_str(&format!("{path}\t{state}\n"));
        }
        (0, out)
    }

    /// Ask units to re-read their own configuration.
    ///
    /// A unit with no `reloadCommand` is restarted instead. That is the honest
    /// fallback: "reload" means the new configuration is in effect, and for a
    /// program with no way to be told, the only way to make that true is to
    /// start it again. Saying so per unit is what keeps an instance-wide reload
    /// from looking like it did something it did not.
    fn reload_unit(&mut self, client: &mut Client, name: &str) -> (i32, String) {
        let members = self.resolve(name);
        if members.is_empty() {
            return (1, format!("no unit or instance named {name:?}\n"));
        }
        let mut out = String::new();
        for member in &members {
            let Some(unit) = self.units.get(member) else {
                continue;
            };
            match (unit.file.reload_command.clone(), unit.phase.is_live()) {
                (Some(argv), true) => {
                    self.run_side_command(client, member, "reload", argv);
                    out.push_str(&format!("{member}\treloaded\n"));
                }
                (Some(_), false) => out.push_str(&format!("{member}\tnot running\n")),
                (None, _) => {
                    self.restart(client, member, Cause::Command);
                    out.push_str(&format!("{member}\trestarted\n"));
                }
            }
        }
        (0, out)
    }

    /// Write an instance file, and let it start.
    ///
    /// One command, because the two it replaces were never independent: the
    /// point of instantiating a stack for a worktree is to have it running, and
    /// an instance file with `autostart` — the default — is what starts it.
    /// The write lands in the mirror here, so the load and the reconcile happen
    /// before the answer is sent and the answer names the phase each unit is
    /// actually in, rather than the one it was asked for.
    ///
    /// The expansion is done here first, against the same code that will do it
    /// for real. A file that would not expand is never written: `doctor` naming
    /// a mistake after the fact is no help when the mistake is in a file you
    /// did not type.
    fn instantiate(&mut self, client: &mut Client, request: &Instantiate<'_>) -> (i32, String) {
        let Instantiate {
            stack,
            name,
            assignments,
            force,
            autostart,
            json,
        } = *request;
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return (2, format!("{name:?} is not a usable instance name\n"));
        }
        if !force && (self.instances.contains_key(name) || self.units.contains_key(name)) {
            return (
                1,
                format!("{name} already exists; pass --force to replace it\n"),
            );
        }

        let declarations = match self.declarations_of(stack) {
            Ok(declarations) => declarations,
            Err(err) => return (1, format!("{err}\n")),
        };

        let mut vars: BTreeMap<String, Value> = BTreeMap::new();
        for assignment in assignments {
            let Some((key, value)) = assignment.split_once('=') else {
                return (2, format!("{assignment:?} is not NAME=VALUE\n"));
            };
            vars.insert(key.to_string(), config::scalar(value));
        }

        // `auto` on the port parameter means "the next free block". Resolving
        // it here rather than in the file is deliberate: the file records the
        // number it got, so a reload does not silently move a running stack to
        // a different port.
        if let Some((port_var, decl)) = declarations.vars.iter().find(|(_, d)| d.is_ports())
            && vars.get(port_var).and_then(Value::as_str) == Some("auto")
        {
            let taken: Vec<(i64, u32)> = self
                .instances
                .values()
                .filter_map(|instance| instance.ports)
                .collect();
            let seed = self
                .instances
                .values()
                .filter(|instance| instance.stack == stack)
                .filter_map(|instance| instance.ports.map(|(base, _)| base))
                .min();
            match config::next_port_block(&taken, seed, decl.span) {
                Some(base) => {
                    vars.insert(port_var.clone(), json!(base));
                }
                None => {
                    return (
                        1,
                        format!(
                            "nothing to allocate {port_var} from: give the first \
                             instance of {stack:?} a number\n"
                        ),
                    );
                }
            }
        }

        let instance = InstanceFile {
            stack: stack.to_string(),
            vars: vars.clone(),
            omit: Vec::new(),
            autostart,
        };
        let expansion = match self.expand(name, &instance, &self.config_files()) {
            Ok(expansion) => expansion,
            Err(err) => return (1, format!("{name} would not load: {err}\n")),
        };

        let mut body = json!({ "stack": stack, "vars": vars });
        if !autostart && let Some(map) = body.as_object_mut() {
            map.insert("autostart".into(), json!(false));
        }
        let mut text = match serde_json::to_string_pretty(&body) {
            Ok(text) => text,
            Err(err) => return (1, format!("{err}\n")),
        };
        text.push('\n');
        let file = format!("{name}.json");
        if let Err(err) = self.write_config(client, &file, text.as_bytes(), !force) {
            return (1, format!("{err}\n"));
        }
        // The write is already in the mirror, so this is the load the watch
        // would have done in a settle window. `load` only decides what should
        // run; `reconcile` is what starts it, and the main loop would not get
        // there until after this answer was sent — so a command that reported
        // `stopped` for everything it had just started would be telling the
        // truth about a state one line of its own making away from over.
        self.load(client);
        self.reconcile(client);

        let phase = |unit: Option<&Unit>| {
            unit.map_or("missing", |unit| unit.phase.as_str())
                .to_string()
        };
        if json {
            return (
                0,
                line(json!({
                    "instance": name,
                    "stack": stack,
                    "file": file,
                    "vars": vars,
                    "units": expansion
                        .members
                        .iter()
                        .map(|member| json!({
                            "name": member,
                            "phase": phase(self.units.get(member)),
                        }))
                        .collect::<Vec<_>>(),
                })),
            );
        }
        let mut out = format!("wrote {file}\n");
        for member in &expansion.members {
            out.push_str(&format!("{member}\t{}\n", phase(self.units.get(member))));
        }
        (0, out)
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
            return line(json!({
                "instances": self
                    .instances
                    .iter()
                    .map(|(name, instance)| json!({
                        "name": name,
                        "stack": instance.stack,
                        "ready": self.ready_count(instance),
                        "total": instance.members.len(),
                    }))
                    .collect::<Vec<_>>(),
                "units": self.units.values().map(|u| self.unit_json(u)).collect::<Vec<_>>(),
            }));
        }

        let mut out = String::from("NAME\tPHASE\tPTY\tRESTARTS\tDESCRIPTION\n");
        for (name, unit) in &self.units {
            if unit.instance.is_some() {
                continue;
            }
            out.push_str(&self.unit_row(name, unit));
        }
        for (name, instance) in &self.instances {
            out.push_str(&format!(
                "{name}\t—\t-\t-\t{}, {}/{} ready\n",
                instance.stack,
                self.ready_count(instance),
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

    fn ready_count(&self, instance: &super::Instance) -> usize {
        instance
            .members
            .iter()
            .filter(|member| {
                self.units
                    .get(*member)
                    .is_some_and(|unit| unit.phase.is_ready())
            })
            .count()
    }

    fn unit_row(&self, name: &str, unit: &Unit) -> String {
        format!(
            "{name}\t{}\t{}\t{}\t{}\n",
            unit.phase.as_str(),
            unit.pty.map_or_else(|| "-".into(), |p| p.to_string()),
            unit.failures,
            unit.file.description.clone().unwrap_or_default()
        )
    }

    fn unit_json(&self, unit: &Unit) -> Value {
        let mut object = json!({
            "name": unit.name,
            "phase": unit.phase.as_str(),
            "restarts": unit.failures,
            "requires": unit.file.requires,
        });
        let map = object.as_object_mut().expect("built as an object");
        if let Some(instance) = &unit.instance {
            map.insert("instance".into(), json!(instance));
        }
        if let Some(pty) = unit.pty {
            map.insert("pty".into(), json!(pty));
        }
        if let Some(exit) = unit.last_exit {
            map.insert("lastExit".into(), json!(exit));
        }
        if let Some(description) = &unit.file.description {
            map.insert("description".into(), json!(description));
        }
        let surfaces: Vec<Value> = self
            .surfaces_of(&unit.name)
            .into_iter()
            .map(|(id, surface)| {
                json!({
                    "id": id,
                    "title": surface.title,
                    "width": surface.width,
                    "height": surface.height,
                })
            })
            .collect();
        if !surfaces.is_empty() {
            map.insert("surfaces".into(), json!(surfaces));
        }
        object
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
            let mut object = self.unit_json(unit);
            object.as_object_mut().expect("object").insert(
                "runs".into(),
                json!(
                    unit.runs
                        .iter()
                        .map(|run| json!({
                            "pty": run.pty,
                            "seq": run.seq,
                            "exitCode": run.exit_code,
                            "endedMs": run.ended_ms,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
            return (0, line(object));
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
        for (id, surface) in self.surfaces_of(name) {
            out.push_str(&format!(
                "surface\t{id}\t{}x{}\t{}\n",
                surface.width, surface.height, surface.title
            ));
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

        // `-u api` is one unit; `-u epic` is every unit of that instance, which
        // is what `epic/` prefixes read as anyway.
        let matches = |record: &&Record| {
            unit_filter.as_ref().is_none_or(|want| {
                let want = want.trim_end_matches('/');
                record.unit == *want || record.instance.as_deref() == Some(want)
            })
        };
        // Without a cursor this is a tail, so take from the newest end rather
        // than collecting the whole ring in order to reverse it.
        let mut selected: Vec<&Record> = match since {
            Some(seq) => self.journal.since(seq).filter(matches).collect(),
            None => self
                .journal
                .tail(usize::MAX)
                .rev()
                .filter(matches)
                .take(count)
                .collect(),
        };
        if since.is_none() {
            selected.reverse();
        }
        if json {
            return line(json!(
                selected.iter().map(|r| r.to_json()).collect::<Vec<_>>()
            ));
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
        if let Some((instance_name, template)) = supervisor::unqualify(name)
            && let Some(instance) = self.instances.get(instance_name)
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
        let home = self.home.clone();
        let cwd = super::expand_tilde(unit.file.cwd.as_deref().unwrap_or("~"), &home);
        let env = match self.resolve_env(client, name, &cwd) {
            Ok(resolved) => resolved.vars,
            Err(failure) => return (1, format!("{failure}\n")),
        };
        if json {
            return (
                0,
                line(json!(
                    env.iter()
                        .map(|(key, value, origin)| {
                            let mut entry = json!({ "key": key, "from": origin.label() });
                            if values {
                                entry
                                    .as_object_mut()
                                    .expect("object")
                                    .insert("value".into(), json!(value));
                            }
                            entry
                        })
                        .collect::<Vec<_>>()
                )),
            );
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
            return line(json!(
                self.stacks
                    .iter()
                    .map(|(name, stack)| json!({
                        "name": name,
                        "vars": stack
                            .vars
                            .iter()
                            .map(|(var, decl)| json!({
                                "name": var,
                                "required": decl.required,
                                "kind": decl.kind,
                            }))
                            .collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>()
            ));
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

        let code = i32::from(!findings.is_empty());
        if json {
            return (
                code,
                line(json!(
                    findings
                        .iter()
                        .map(|(where_, what)| json!({ "where": where_, "what": what }))
                        .collect::<Vec<_>>()
                )),
            );
        }
        if findings.is_empty() {
            return (0, String::from("no findings\n"));
        }
        let mut out = String::new();
        for (where_, what) in &findings {
            out.push_str(&format!("{where_}\t{what}\n"));
        }
        (code, out)
    }
}

/// One JSON value as a line, which is what both the CLI and the journal emit.
fn line(value: Value) -> String {
    format!("{value}\n")
}
