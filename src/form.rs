//! The connection form: fields, editing state and validation.
//!
//! Deliberately free of rendering and I/O so the rules are testable.

use crate::config::{Connection, Engine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    Project,
    Name,
    Engine,
    Host,
    Port,
    Database,
    User,
    Password,
    SslMode,
    TrustCert,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Secret,
    Select,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormMode {
    NewConnection,
    EditConnection { project: String, name: String },
    NewProject,
    RenameProject { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TestState {
    #[default]
    Idle,
    Running,
    Ok(String),
    Failed(String, Vec<String>),
}

pub const ENGINES: [Engine; 2] = [Engine::Postgres, Engine::Mssql];
pub const SSL_MODES: [&str; 3] = ["disable", "prefer", "require"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub mode: FormMode,
    pub project: String,
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    /// Newly typed password. Empty and untouched means "leave it alone".
    pub password: String,
    pub password_touched: bool,
    pub has_stored_password: bool,
    pub sslmode: String,
    pub trust_cert: bool,
    pub read_only: bool,

    pub cursor: usize,
    pub editing: bool,
    pub errors: Vec<(FieldId, String)>,
    pub test: TestState,

    /// The connection as it was, so an untouched password source survives.
    orig: Option<Connection>,
    /// Fields still holding a prefilled default. The first keystroke replaces
    /// them rather than appending, so `host` cannot become `127.0.0.1sql.internal`.
    prefilled: Vec<FieldId>,
    /// Existing `(project, name)` pairs for the uniqueness check.
    existing: Vec<(String, String)>,
    pub projects: Vec<String>,
}

impl Form {
    fn base(mode: FormMode, projects: Vec<String>, existing: Vec<(String, String)>) -> Self {
        Self {
            mode,
            project: String::new(),
            name: String::new(),
            engine: Engine::Postgres,
            host: "127.0.0.1".into(),
            port: Engine::Postgres.default_port().to_string(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            password_touched: false,
            has_stored_password: false,
            sslmode: "prefer".into(),
            trust_cert: false,
            read_only: false,
            cursor: 0,
            editing: false,
            errors: Vec::new(),
            test: TestState::Idle,
            prefilled: vec![FieldId::Host, FieldId::Port],
            orig: None,
            existing,
            projects,
        }
    }

    pub fn new_connection(
        project: &str,
        projects: Vec<String>,
        existing: Vec<(String, String)>,
    ) -> Self {
        let mut f = Self::base(FormMode::NewConnection, projects, existing);
        f.project = project.to_string();
        // Land on the name field; the project is usually already right.
        f.cursor = 1;
        f
    }

    pub fn edit_connection(
        project: &str,
        c: &Connection,
        projects: Vec<String>,
        existing: Vec<(String, String)>,
    ) -> Self {
        let mut f = Self::base(
            FormMode::EditConnection {
                project: project.to_string(),
                name: c.name.clone(),
            },
            projects,
            existing,
        );
        f.project = project.to_string();
        f.name = c.name.clone();
        f.engine = c.engine;
        f.host = c.host.clone();
        f.port = c.port_or_default().to_string();
        f.database = c.database.clone();
        f.user = c.user.clone();
        f.sslmode = c.sslmode.clone().unwrap_or_else(|| "prefer".into());
        f.trust_cert = c.trust_cert;
        f.read_only = c.read_only;
        f.has_stored_password =
            c.keyring.is_some() || c.password_command.is_some() || c.password.is_some();
        f.orig = Some(c.clone());
        f.prefilled.clear();
        f.cursor = 1;
        f
    }

    pub fn new_project(projects: Vec<String>) -> Self {
        Self::base(FormMode::NewProject, projects, Vec::new())
    }

    pub fn rename_project(name: &str, projects: Vec<String>) -> Self {
        let mut f = Self::base(
            FormMode::RenameProject {
                name: name.to_string(),
            },
            projects,
            Vec::new(),
        );
        f.project = name.to_string();
        f
    }

    pub fn is_project_only(&self) -> bool {
        matches!(
            self.mode,
            FormMode::NewProject | FormMode::RenameProject { .. }
        )
    }

    pub fn title(&self) -> String {
        match &self.mode {
            FormMode::NewConnection => " new connection ".into(),
            FormMode::EditConnection { project, name } => format!(" edit {project}/{name} "),
            FormMode::NewProject => " new project ".into(),
            FormMode::RenameProject { name } => format!(" rename project {name} "),
        }
    }

    pub fn fields(&self) -> Vec<FieldId> {
        use FieldId::*;
        if self.is_project_only() {
            return vec![Project];
        }
        let mut v = vec![Project, Name, Engine, Host, Port, Database, User, Password];
        if self.engine == crate::config::Engine::Postgres {
            v.push(SslMode);
        }
        v.push(TrustCert);
        v.push(ReadOnly);
        v
    }

    pub fn kind(&self, id: FieldId) -> FieldKind {
        use FieldId::*;
        match id {
            Password => FieldKind::Secret,
            Engine | SslMode => FieldKind::Select,
            TrustCert | ReadOnly => FieldKind::Toggle,
            _ => FieldKind::Text,
        }
    }

    pub fn label(&self, id: FieldId) -> &'static str {
        use FieldId::*;
        match id {
            Project => {
                if self.is_project_only() {
                    "project name"
                } else {
                    "project"
                }
            }
            Name => "name",
            Engine => "engine",
            Host => "host",
            Port => "port",
            Database => "database",
            User => "user",
            Password => "password",
            SslMode => "sslmode",
            TrustCert => "trust cert",
            ReadOnly => "read only",
        }
    }

    /// What to draw for a field, secrets already masked.
    pub fn display(&self, id: FieldId) -> String {
        use FieldId::*;
        match id {
            Project => self.project.clone(),
            Name => self.name.clone(),
            Engine => self.engine.label().to_string(),
            Host => self.host.clone(),
            Port => self.port.clone(),
            Database => self.database.clone(),
            User => self.user.clone(),
            Password => {
                if self.password_touched {
                    "•".repeat(self.password.chars().count())
                } else if self.has_stored_password {
                    "(unchanged)".into()
                } else {
                    String::new()
                }
            }
            SslMode => self.sslmode.clone(),
            TrustCert => bool_label(self.trust_cert),
            ReadOnly => bool_label(self.read_only),
        }
    }

    pub fn current(&self) -> FieldId {
        let f = self.fields();
        f[self.cursor.min(f.len() - 1)]
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.fields().len() as isize;
        self.cursor = (((self.cursor as isize + delta) % len + len) % len) as usize;
    }

    pub fn error_for(&self, id: FieldId) -> Option<&str> {
        self.errors
            .iter()
            .find(|(f, _)| *f == id)
            .map(|(_, m)| m.as_str())
    }

    // --- editing ---

    pub fn text_mut(&mut self, id: FieldId) -> Option<&mut String> {
        use FieldId::*;
        Some(match id {
            Project => &mut self.project,
            Name => &mut self.name,
            Host => &mut self.host,
            Port => &mut self.port,
            Database => &mut self.database,
            User => &mut self.user,
            Password => &mut self.password,
            _ => return None,
        })
    }

    pub fn type_char(&mut self, c: char) {
        let id = self.current();
        if id == FieldId::Port && !c.is_ascii_digit() {
            return;
        }
        if id == FieldId::Password && !self.password_touched {
            self.password_touched = true;
            self.password.clear();
        }
        let replace = self.take_prefilled(id);
        if let Some(s) = self.text_mut(id) {
            if replace {
                s.clear();
            }
            s.push(c);
        }
    }

    /// True when this field still holds an untouched default, which the caller
    /// should replace rather than append to. Clears the mark either way.
    fn take_prefilled(&mut self, id: FieldId) -> bool {
        match self.prefilled.iter().position(|f| *f == id) {
            Some(i) => {
                self.prefilled.remove(i);
                true
            }
            None => false,
        }
    }

    /// Empties the field under the cursor.
    pub fn clear_field(&mut self) {
        let id = self.current();
        self.take_prefilled(id);
        if id == FieldId::Password {
            self.password_touched = true;
        }
        if let Some(s) = self.text_mut(id) {
            s.clear();
        }
    }

    pub fn backspace(&mut self) {
        let id = self.current();
        // Backspacing a default means "edit it", so drop the replace-on-type mark.
        self.take_prefilled(id);
        if id == FieldId::Password && !self.password_touched {
            // The first edit clears the placeholder rather than the stored value.
            self.password_touched = true;
            self.password.clear();
            return;
        }
        if let Some(s) = self.text_mut(id) {
            s.pop();
        }
    }

    /// Cycles a select or flips a toggle. Text fields are unaffected.
    pub fn cycle(&mut self, delta: isize) {
        match self.current() {
            FieldId::Engine => {
                let i = ENGINES.iter().position(|e| *e == self.engine).unwrap_or(0);
                let n = ENGINES.len() as isize;
                let next = (((i as isize + delta) % n + n) % n) as usize;
                let old_default = self.engine.default_port().to_string();
                self.engine = ENGINES[next];
                // Follow the engine's default port unless the user set their own.
                if self.port.is_empty() || self.port == old_default {
                    self.port = self.engine.default_port().to_string();
                }
                self.cursor = self.cursor.min(self.fields().len() - 1);
            }
            FieldId::SslMode => {
                let i = SSL_MODES
                    .iter()
                    .position(|m| *m == self.sslmode)
                    .unwrap_or(1);
                let n = SSL_MODES.len() as isize;
                let next = (((i as isize + delta) % n + n) % n) as usize;
                self.sslmode = SSL_MODES[next].to_string();
            }
            FieldId::TrustCert => self.trust_cert = !self.trust_cert,
            FieldId::ReadOnly => self.read_only = !self.read_only,
            _ => {}
        }
    }

    // --- validation ---

    pub fn validate(&mut self) -> bool {
        use FieldId::*;
        let mut errs: Vec<(FieldId, String)> = Vec::new();
        fn require(errs: &mut Vec<(FieldId, String)>, id: FieldId, v: &str, label: &str) {
            if v.trim().is_empty() {
                errs.push((id, format!("{label} is required")));
            }
        }
        require(&mut errs, Project, &self.project, "project");

        if self.project.contains('/') {
            errs.push((Project, "project cannot contain `/`".into()));
        }
        if self.is_project_only() {
            let taken = match &self.mode {
                FormMode::RenameProject { name } => name != &self.project,
                _ => true,
            };
            if taken && self.projects.iter().any(|p| p == &self.project) {
                errs.push((Project, "a project with that name already exists".into()));
            }
            self.errors = errs;
            return self.errors.is_empty();
        }

        require(&mut errs, Name, &self.name, "name");
        require(&mut errs, Host, &self.host, "host");
        require(&mut errs, Database, &self.database, "database");
        require(&mut errs, User, &self.user, "user");

        if self.name.contains('/') {
            errs.push((Name, "name cannot contain `/`".into()));
        }
        match self.port.parse::<u16>() {
            Ok(0) | Err(_) => errs.push((Port, "port must be 1–65535".into())),
            Ok(_) => {}
        }
        if self.engine == crate::config::Engine::Postgres
            && !SSL_MODES.contains(&self.sslmode.as_str())
        {
            errs.push((SslMode, "must be disable, prefer or require".into()));
        }

        // project/name is the keyring account, so the pair must be unique.
        let is_self = match &self.mode {
            FormMode::EditConnection { project, name } => {
                project == &self.project && name == &self.name
            }
            _ => false,
        };
        if !is_self
            && self
                .existing
                .iter()
                .any(|(p, n)| p == &self.project && n == &self.name)
        {
            errs.push((
                Name,
                format!("{}/{} already exists", self.project, self.name),
            ));
        }

        if !self.has_stored_password && !self.password_touched {
            errs.push((
                Password,
                "no password yet — type one, or set password_command in the config".into(),
            ));
        }

        self.errors = errs;
        self.errors.is_empty()
    }

    pub fn keyring_account(&self) -> String {
        crate::store::keyring_account(self.project.trim(), self.name.trim())
    }

    /// The connection this form describes. Preserves the original password
    /// source when the password field was not touched.
    pub fn to_connection(&self) -> Connection {
        let mut c = self.orig.clone().unwrap_or(Connection {
            name: String::new(),
            engine: self.engine,
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            keyring: None,
            password_command: None,
            password: None,
            sslmode: None,
            trust_cert: false,
            read_only: false,
        });
        c.name = self.name.trim().to_string();
        c.engine = self.engine;
        c.host = self.host.trim().to_string();
        c.port = self.port.parse::<u16>().ok();
        c.database = self.database.trim().to_string();
        c.user = self.user.trim().to_string();
        c.sslmode = if self.engine == Engine::Postgres {
            Some(self.sslmode.clone())
        } else {
            None
        };
        c.trust_cert = self.trust_cert;
        c.read_only = self.read_only;
        if self.password_touched {
            // A typed password goes to the keyring; drop the other sources so
            // there is exactly one place the secret lives.
            c.keyring = Some(self.keyring_account());
            c.password_command = None;
            c.password = None;
        } else if c.keyring.is_some() {
            // Keep the account in step with a rename or a move.
            c.keyring = Some(self.keyring_account());
        }
        c
    }

    /// Whether saving needs to write the keyring, and what to write.
    pub fn password_to_store(&self) -> Option<(String, String)> {
        if self.password_touched && !self.password.is_empty() {
            Some((self.keyring_account(), self.password.clone()))
        } else {
            None
        }
    }

    /// The old keyring account to clean up after a rename or move.
    pub fn stale_keyring_account(&self) -> Option<String> {
        let FormMode::EditConnection { project, name } = &self.mode else {
            return None;
        };
        let old = self.orig.as_ref()?.keyring.as_ref()?;
        let expected = crate::store::keyring_account(project, name);
        if old == &expected && expected != self.keyring_account() {
            Some(expected)
        } else {
            None
        }
    }
}

fn bool_label(v: bool) -> String {
    if v { "yes" } else { "no" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing() -> Vec<(String, String)> {
        vec![
            ("Acme".into(), "local".into()),
            ("Beta".into(), "local".into()),
        ]
    }

    fn projects() -> Vec<String> {
        vec!["Acme".into(), "Beta".into()]
    }

    fn filled() -> Form {
        let mut f = Form::new_connection("Acme", projects(), existing());
        f.name = "warehouse".into();
        f.host = "db.internal".into();
        f.database = "acme".into();
        f.user = "svc".into();
        f.password = "pw".into();
        f.password_touched = true;
        f
    }

    fn conn() -> Connection {
        Connection {
            name: "local".into(),
            engine: Engine::Postgres,
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "acme".into(),
            user: "postgres".into(),
            keyring: Some("Acme/local".into()),
            password_command: None,
            password: None,
            sslmode: Some("disable".into()),
            trust_cert: false,
            read_only: false,
        }
    }

    #[test]
    fn new_form_starts_on_the_name_field_with_the_project_prefilled() {
        let f = Form::new_connection("Acme", projects(), existing());
        assert_eq!(f.project, "Acme");
        assert_eq!(f.current(), FieldId::Name);
        assert_eq!(f.port, "5432");
    }

    #[test]
    fn engine_specific_fields_appear_and_disappear() {
        let mut f = filled();
        assert!(f.fields().contains(&FieldId::SslMode));
        f.engine = Engine::Mssql;
        assert!(!f.fields().contains(&FieldId::SslMode));
        assert!(f.fields().contains(&FieldId::TrustCert));
    }

    #[test]
    fn switching_engine_follows_the_default_port() {
        let mut f = filled();
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Engine)
            .unwrap();
        f.cycle(1);
        assert_eq!(f.engine, Engine::Mssql);
        assert_eq!(f.port, "1433");
        f.cycle(-1);
        assert_eq!(f.port, "5432");
    }

    #[test]
    fn a_custom_port_survives_an_engine_switch() {
        let mut f = filled();
        f.port = "6543".into();
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Engine)
            .unwrap();
        f.cycle(1);
        assert_eq!(f.port, "6543");
    }

    #[test]
    fn cursor_stays_valid_when_a_field_disappears() {
        let mut f = filled();
        f.cursor = f.fields().len() - 1;
        let engine_at = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Engine)
            .unwrap();
        f.cursor = engine_at;
        f.cycle(1); // mssql drops sslmode
        assert!(f.cursor < f.fields().len());
        f.cursor = f.fields().len() - 1;
        assert_eq!(f.current(), FieldId::ReadOnly);
    }

    #[test]
    fn typing_replaces_a_prefilled_default_instead_of_appending() {
        let mut f = Form::new_connection("Acme", projects(), existing());
        f.cursor = f.fields().iter().position(|x| *x == FieldId::Host).unwrap();
        for c in "sql.internal".chars() {
            f.type_char(c);
        }
        assert_eq!(f.host, "sql.internal");
        // Only the first keystroke replaces; the rest append.
        f.type_char('!');
        assert_eq!(f.host, "sql.internal!");
    }

    #[test]
    fn backspacing_a_default_edits_it_rather_than_replacing_it() {
        let mut f = Form::new_connection("Acme", projects(), existing());
        f.cursor = f.fields().iter().position(|x| *x == FieldId::Host).unwrap();
        f.backspace();
        assert_eq!(f.host, "127.0.0.");
        f.type_char('9');
        assert_eq!(f.host, "127.0.0.9");
    }

    #[test]
    fn real_values_are_never_treated_as_defaults() {
        let mut f = Form::edit_connection("Acme", &conn(), projects(), existing());
        f.cursor = f.fields().iter().position(|x| *x == FieldId::Host).unwrap();
        f.type_char('9');
        assert_eq!(f.host, "127.0.0.19", "an edit must append, not replace");
    }

    #[test]
    fn clear_field_empties_the_current_field() {
        let mut f = filled();
        f.cursor = f.fields().iter().position(|x| *x == FieldId::Host).unwrap();
        f.clear_field();
        assert!(f.host.is_empty());
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Password)
            .unwrap();
        f.clear_field();
        assert!(f.password.is_empty());
        assert!(f.password_touched);
    }

    #[test]
    fn port_field_rejects_non_digits() {
        let mut f = filled();
        f.cursor = f.fields().iter().position(|x| *x == FieldId::Port).unwrap();
        f.clear_field();
        for c in "54a3x2".chars() {
            f.type_char(c);
        }
        assert_eq!(f.port, "5432");
    }

    #[test]
    fn validation_flags_every_missing_field() {
        let mut f = Form::new_connection("", projects(), existing());
        f.host.clear(); // `base` prefills a sensible default
        assert!(!f.validate());
        for id in [
            FieldId::Project,
            FieldId::Name,
            FieldId::Host,
            FieldId::Database,
            FieldId::User,
            FieldId::Password,
        ] {
            assert!(f.error_for(id).is_some(), "{id:?} should be flagged");
        }
    }

    #[test]
    fn validation_passes_on_a_complete_form() {
        let mut f = filled();
        assert!(f.validate(), "{:?}", f.errors);
    }

    #[test]
    fn duplicate_project_and_name_is_rejected() {
        let mut f = filled();
        f.name = "local".into();
        assert!(!f.validate());
        assert!(f.error_for(FieldId::Name).unwrap().contains("Acme/local"));
    }

    #[test]
    fn the_same_name_in_another_project_is_allowed() {
        let mut f = filled();
        f.project = "Gamma".into();
        f.name = "local".into();
        assert!(f.validate(), "{:?}", f.errors);
    }

    #[test]
    fn slashes_are_rejected_because_they_split_the_keyring_account() {
        let mut f = filled();
        f.name = "a/b".into();
        assert!(!f.validate());
        assert!(f.error_for(FieldId::Name).unwrap().contains('/'));
    }

    #[test]
    fn editing_keeps_its_own_name_available() {
        let mut f = Form::edit_connection("Acme", &conn(), projects(), existing());
        assert!(f.validate(), "{:?}", f.errors);
        assert_eq!(f.current(), FieldId::Name);
    }

    #[test]
    fn editing_needs_no_password_when_one_is_already_stored() {
        let mut f = Form::edit_connection("Acme", &conn(), projects(), existing());
        assert!(f.has_stored_password);
        assert_eq!(f.display(FieldId::Password), "(unchanged)");
        assert!(f.validate());
        assert!(f.password_to_store().is_none());
    }

    #[test]
    fn typing_replaces_the_placeholder_rather_than_appending() {
        let mut f = Form::edit_connection("Acme", &conn(), projects(), existing());
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Password)
            .unwrap();
        f.type_char('x');
        assert_eq!(f.password, "x");
        assert_eq!(f.display(FieldId::Password), "•");
        assert_eq!(
            f.password_to_store(),
            Some(("Acme/local".into(), "x".into()))
        );
    }

    #[test]
    fn an_untouched_password_command_is_preserved() {
        let mut c = conn();
        c.keyring = None;
        c.password_command = Some("op read op://x/y".into());
        let f = Form::edit_connection("Acme", &c, projects(), existing());
        let out = f.to_connection();
        assert_eq!(out.password_command.as_deref(), Some("op read op://x/y"));
        assert!(out.keyring.is_none());
    }

    #[test]
    fn typing_a_password_moves_the_secret_to_the_keyring() {
        let mut c = conn();
        c.keyring = None;
        c.password_command = Some("op read op://x/y".into());
        let mut f = Form::edit_connection("Acme", &c, projects(), existing());
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::Password)
            .unwrap();
        f.type_char('p');
        let out = f.to_connection();
        assert_eq!(out.keyring.as_deref(), Some("Acme/local"));
        assert!(out.password_command.is_none());
    }

    #[test]
    fn renaming_moves_the_keyring_account_and_reports_the_stale_one() {
        let mut f = Form::edit_connection("Acme", &conn(), projects(), existing());
        f.name = "renamed".into();
        assert_eq!(f.to_connection().keyring.as_deref(), Some("Acme/renamed"));
        assert_eq!(f.stale_keyring_account().as_deref(), Some("Acme/local"));
    }

    #[test]
    fn no_stale_account_when_nothing_moved() {
        let f = Form::edit_connection("Acme", &conn(), projects(), existing());
        assert!(f.stale_keyring_account().is_none());
    }

    #[test]
    fn mssql_connections_carry_no_sslmode() {
        let mut f = filled();
        f.engine = Engine::Mssql;
        assert!(f.to_connection().sslmode.is_none());
    }

    #[test]
    fn project_forms_show_one_field_and_reject_duplicates() {
        let mut f = Form::new_project(projects());
        assert_eq!(f.fields(), vec![FieldId::Project]);
        assert_eq!(f.label(FieldId::Project), "project name");
        f.project = "Acme".into();
        assert!(!f.validate());
        f.project = "Gamma".into();
        assert!(f.validate(), "{:?}", f.errors);
    }

    #[test]
    fn renaming_a_project_to_its_own_name_is_fine() {
        let mut f = Form::rename_project("Acme", projects());
        assert!(f.validate(), "{:?}", f.errors);
        f.project = "Beta".into();
        assert!(!f.validate());
    }

    #[test]
    fn toggles_flip_and_selects_wrap() {
        let mut f = filled();
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::ReadOnly)
            .unwrap();
        f.cycle(1);
        assert!(f.read_only);
        assert_eq!(f.display(FieldId::ReadOnly), "yes");
        f.cursor = f
            .fields()
            .iter()
            .position(|x| *x == FieldId::SslMode)
            .unwrap();
        f.sslmode = "require".into();
        f.cycle(1);
        assert_eq!(f.sslmode, "disable");
    }

    #[test]
    fn cursor_wraps_both_ways() {
        let mut f = filled();
        f.cursor = 0;
        f.move_cursor(-1);
        assert_eq!(f.cursor, f.fields().len() - 1);
        f.move_cursor(1);
        assert_eq!(f.cursor, 0);
    }
}
