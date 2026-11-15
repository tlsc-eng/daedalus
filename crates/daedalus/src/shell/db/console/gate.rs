use daedalus_db::classify::{Kind, Statement};

pub(super) struct Target<'a> {
    pub name: &'a str,
    pub read_only: bool,
    pub production: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum Gate {
    Run,
    Confirm {
        title: String,
        detail: String,
    },
    /// An empty message means there was nothing to run and nothing to say.
    Refuse(String),
}

pub(super) fn excerpt(sql: &str, chars: usize) -> String {
    let line = sql.trim().lines().next().unwrap_or("").trim();
    match line.chars().count() > chars {
        true => format!("{}…", line.chars().take(chars).collect::<String>()),
        false => line.to_owned(),
    }
}

/// Decides whether the statements may run, need a confirmation, or are refused outright.
pub(super) fn gate(sql: &str, statements: &[Statement], target: &Target) -> Gate {
    let name = target.name;
    if statements.is_empty() {
        return Gate::Refuse(String::new());
    }
    if statements.iter().any(|s| s.kind == Kind::Transaction) {
        return Gate::Refuse(
            "BEGIN, COMMIT, ROLLBACK and SAVEPOINT can't run in the console. Use Manual mode and its Commit and Roll Back buttons."
                .into(),
        );
    }
    if target.read_only {
        if statements.iter().any(|s| s.kind == Kind::Unknown) {
            return Gate::Refuse(format!(
                "{name} is read-only, and Daedalus can't tell what one of these statements does, so none of them run."
            ));
        }
        if statements.iter().any(|s| s.kind != Kind::Read) {
            return Gate::Refuse(format!(
                "{name} is read-only, so only statements that read data can run."
            ));
        }
    }
    let mut reasons = Vec::new();
    if target.production && statements.iter().any(|s| s.kind != Kind::Read) {
        reasons.push(format!("{name} is marked as production."));
    }
    for s in statements.iter().filter(|s| s.unguarded).take(3) {
        let text = sql.get(s.range.clone()).unwrap_or("");
        reasons.push(format!(
            "{} has no WHERE clause, so it changes every row.",
            excerpt(text, 60)
        ));
    }
    if reasons.is_empty() {
        return Gate::Run;
    }
    let title = match target.production {
        true => format!("Change data on {name}?"),
        false => "Change every row?".into(),
    };
    Gate::Confirm {
        title,
        detail: reasons.join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daedalus_db::Engine;
    use daedalus_db::classify::split;

    fn target(read_only: bool, production: bool) -> Target<'static> {
        Target {
            name: "main",
            read_only,
            production,
        }
    }

    fn check(sql: &str, t: &Target) -> Gate {
        gate(sql, &split(sql, Engine::Postgres), t)
    }

    #[test]
    fn read_only_runs_reads() {
        assert_eq!(check("select 1; select 2", &target(true, false)), Gate::Run);
    }

    #[test]
    fn read_only_refuses_a_write() {
        assert!(matches!(
            check("select 1; delete from t where id = 1", &target(true, false)),
            Gate::Refuse(m) if m.contains("only statements that read data")
        ));
    }

    #[test]
    fn read_only_refuses_session_statements() {
        let sql = "set search_path = x";
        assert_eq!(split(sql, Engine::Postgres)[0].kind, Kind::Session);
        assert!(matches!(check(sql, &target(true, false)), Gate::Refuse(_)));
    }

    #[test]
    fn read_only_refuses_a_script_with_an_unknown_chunk() {
        let sql = "select 1; do $$ begin null; end $$;";
        assert!(
            split(sql, Engine::Postgres)
                .iter()
                .any(|s| s.kind == Kind::Unknown)
        );
        assert!(matches!(
            check(sql, &target(true, false)),
            Gate::Refuse(m) if m.contains("can't tell")
        ));
    }

    #[test]
    fn transaction_statements_are_refused() {
        let sql = "begin";
        assert_eq!(split(sql, Engine::Postgres)[0].kind, Kind::Transaction);
        assert!(matches!(
            check(sql, &target(false, false)),
            Gate::Refuse(m) if m.contains("Manual mode")
        ));
    }

    #[test]
    fn production_write_asks() {
        let sql = "insert into t values (1)";
        assert!(matches!(
            check(sql, &target(false, true)),
            Gate::Confirm { title, .. } if title == "Change data on main?"
        ));
    }

    #[test]
    fn production_read_runs() {
        assert_eq!(check("select 1", &target(false, true)), Gate::Run);
    }

    #[test]
    fn unguarded_delete_asks_on_any_connection() {
        let sql = "delete from t";
        assert!(matches!(
            check(sql, &target(false, false)),
            Gate::Confirm { title, detail } if title == "Change every row?" && detail.contains("delete from t")
        ));
    }

    #[test]
    fn production_asks_for_a_mixed_script() {
        assert!(matches!(
            check("select 1; update t set a = 1", &target(false, true)),
            Gate::Confirm { title, detail }
                if title == "Change data on main?" && detail.contains("production")
        ));
    }

    #[test]
    fn unguarded_asks_for_a_mixed_script() {
        assert!(matches!(
            check("select 1; update t set a = 1", &target(false, false)),
            Gate::Confirm { title, detail }
                if title == "Change every row?" && detail.contains("update t set a = 1")
        ));
    }

    #[test]
    fn a_transaction_statement_in_a_script_is_refused() {
        assert!(matches!(
            check("select 1; commit; select 2", &target(false, false)),
            Gate::Refuse(m) if m.contains("Manual mode")
        ));
    }

    #[test]
    fn plain_write_runs() {
        assert_eq!(
            check("update t set a = 1 where id = 2", &target(false, false)),
            Gate::Run
        );
    }

    #[test]
    fn nothing_to_run_is_a_silent_refusal() {
        assert_eq!(
            check("  ", &target(false, false)),
            Gate::Refuse(String::new())
        );
    }
}
