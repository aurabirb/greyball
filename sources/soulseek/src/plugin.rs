//! `core::Plugin` impl for Soulseek; `probe()` starts a cooldown-guarded background check (slskd, Docker) and reports its last result.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{Bus, CoreEvent, MediaProvider, Plugin, PluginHealth, SetupLog, SetupPrompt, Source, SourceId, Wiring, expand_home, tilde};

use crate::client::{SlskdClient, SlskdConfig};
use crate::docker;
use crate::persist::{self, State};
use crate::source::SoulseekSource;
use crate::yaml_config::{self, YamlAuth};

const CHECK_COOLDOWN: Duration = Duration::from_secs(30);
const STARTUP_WAIT: Duration = Duration::from_secs(60);
const DEFAULT_FOLDER: &str = "~/Documents/slskd";

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Location,
    Folder,
    SoulseekUser,
    SoulseekPass,
    Confirm,
    User,
    Pass,
    Host,
}

/// What decides the question list, frozen once the first answer is in so later answers can't shift it.
#[derive(Clone, Copy, Default)]
struct Facts {
    docker_offer: bool,
    unreachable: bool,
}

#[derive(Clone)]
struct DockerSnapshot {
    status: docker::Status,
    container: Option<String>,
}

pub struct SoulseekPlugin {
    conn: Mutex<SlskdConfig>,
    cache_dir: PathBuf,
    media_cache_dir: PathBuf,
    bus: Bus,
    state: Mutex<State>,
    yaml_auth: Mutex<Option<YamlAuth>>,
    reachable: Arc<Mutex<Option<bool>>>,
    docker: Arc<Mutex<Option<DockerSnapshot>>>,
    facts: Mutex<Facts>,
    checking: Arc<AtomicBool>,
    next_check: Arc<Mutex<Instant>>,
}

impl SoulseekPlugin {
    pub fn new(
        mut conn: SlskdConfig,
        configured_data_dir: Option<String>,
        cache_dir: PathBuf,
        media_cache_dir: PathBuf,
        bus: Bus,
    ) -> Self {
        let mut state = persist::load(&cache_dir);
        if let Some(dir) = configured_data_dir.filter(|s| !s.trim().is_empty()) {
            state.data_dir = Some(dir);
        }
        if let Some(saved) = &state.connection {
            conn.base_url = saved.base_url.clone();
            conn.username = saved.username.clone();
            conn.password = saved.password.clone();
        }
        let yaml_auth = state.data_dir.as_deref().and_then(|d| yaml_config::read(&expand_home(d)));
        Self {
            conn: Mutex::new(conn),
            cache_dir,
            media_cache_dir,
            bus,
            state: Mutex::new(state),
            yaml_auth: Mutex::new(yaml_auth),
            reachable: Arc::new(Mutex::new(None)),
            docker: Arc::new(Mutex::new(None)),
            facts: Mutex::default(),
            checking: Arc::new(AtomicBool::new(false)),
            next_check: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// The configured connection with any override found in the data directory's own `slskd.yml` on top.
    fn effective_conn(&self) -> SlskdConfig {
        let mut conn = self.conn.lock().unwrap().clone();
        if let Some(auth) = self.yaml_auth.lock().unwrap().as_ref() {
            if let Some(key) = &auth.api_key {
                conn.api_key = Some(key.clone());
            } else {
                if let Some(u) = &auth.username {
                    conn.username = u.clone();
                }
                if let Some(p) = &auth.password {
                    conn.password = p.clone();
                }
            }
        }
        conn
    }

    fn kick_off_connectivity_check(&self) {
        if self.checking.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut next = self.next_check.lock().unwrap();
            let already_checked = self.reachable.lock().unwrap().is_some();
            if already_checked && Instant::now() < *next {
                self.checking.store(false, Ordering::SeqCst);
                return;
            }
            *next = Instant::now() + CHECK_COOLDOWN;
        }
        let conn = self.conn.lock().unwrap().clone();
        let (reachable, docker_snapshot) = (self.reachable.clone(), self.docker.clone());
        let checking = self.checking.clone();
        let bus = self.bus.clone();
        std::thread::spawn(move || {
            let ok = SlskdClient::new(conn).reachable();
            let snapshot = snapshot_docker();
            let mut r = reachable.lock().unwrap();
            let mut d = docker_snapshot.lock().unwrap();
            let changed = *r != Some(ok) || d.as_ref().map(|d| (&d.status, &d.container)) != Some((&snapshot.status, &snapshot.container));
            *r = Some(ok);
            *d = Some(snapshot);
            drop((r, d));
            checking.store(false, Ordering::SeqCst);
            if changed {
                bus.send(CoreEvent::PluginStatusChanged);
            }
        });
    }

    fn base_url(&self) -> String {
        self.conn.lock().unwrap().base_url.clone()
    }

    fn unreachable_message(&self) -> String {
        format!(
            "no slskd reachable at {} — select to set it up (with Docker, or from the folder of one you run), \
             or set [soulseek] enabled = false in config.toml to silence this",
            self.base_url()
        )
    }

    fn docker_ready(&self) -> bool {
        self.docker.lock().unwrap().as_ref().is_some_and(|d| d.status == docker::Status::Ready)
    }

    /// Docker can create the container: it is usable and no slskd of the user's own answers.
    fn docker_offer(&self) -> bool {
        let reachable = *self.reachable.lock().unwrap() == Some(true);
        self.docker_ready() && (self.state.lock().unwrap().docker.is_some() || !reachable)
    }

    fn refresh_facts(&self) {
        *self.facts.lock().unwrap() =
            Facts { docker_offer: self.docker_offer(), unreachable: *self.reachable.lock().unwrap() != Some(true) };
    }

    /// The questions for the answers given so far; the full list once they determine the path.
    fn plan(&self, answers: &[String]) -> Vec<Step> {
        let mut steps = vec![Step::Location];
        let Some(location) = answers.first().map(|a| a.trim()) else { return steps };
        let facts = *self.facts.lock().unwrap();
        if !location.is_empty() {
            if expand_home(location).is_dir() && yaml_config::read(&expand_home(location)).is_none() {
                steps.extend([Step::User, Step::Pass]);
            }
            if facts.unreachable {
                steps.push(Step::Host);
            }
        } else if facts.docker_offer {
            if self.state.lock().unwrap().docker.is_none() {
                steps.push(Step::Folder);
                if let Some(folder) = answers.get(1)
                    && !folder_path(folder).join("slskd.yml").exists()
                {
                    steps.extend([Step::SoulseekUser, Step::SoulseekPass]);
                }
            }
            steps.push(Step::Confirm);
        }
        steps
    }

    /// What the Docker path will do, for the last question.
    fn summary(&self, answers: &[String]) -> String {
        let cache = tilde(&self.media_cache_dir);
        match self.state.lock().unwrap().docker.clone() {
            Some(d) => format!(
                "I will recreate the slskd container in Docker, keeping its settings in {} and saving downloads in {cache}. \
                 Press Enter to go ahead, or Esc to stop.",
                d.folder
            ),
            None => {
                let folder = answers.get(1).map_or(DEFAULT_FOLDER, |f| f.trim());
                let login = if folder_path(folder).join("slskd.yml").exists() { "using the settings already in it" } else { "with a new slskd.yml holding your Soulseek login" };
                format!(
                    "I will start slskd in Docker, keeping its settings in {folder} ({login}) and saving downloads in {cache}. \
                     Press Enter to go ahead, or Esc to stop."
                )
            }
        }
    }

    fn health(&self) -> PluginHealth {
        let stale = self.state.lock().unwrap().docker.as_ref().filter(|d| d.mounted_cache != self.media_cache_dir).cloned();
        match *self.reachable.lock().unwrap() {
            None => PluginHealth::Warn("checking for a local slskd…".to_string()),
            Some(false) => PluginHealth::Warn(self.unreachable_message()),
            Some(true) if stale.is_some() => {
                let d = stale.unwrap();
                PluginHealth::Warn(format!(
                    "the media cache is now {} but the slskd container writes downloads to {} — select to recreate it",
                    tilde(&self.media_cache_dir),
                    tilde(&d.mounted_cache)
                ))
            }
            Some(true) if self.state.lock().unwrap().data_dir.is_none() => PluginHealth::Warn(
                "slskd found, but no data directory configured — select to set it up \
                 (search works either way; playback needs it to find finished downloads)"
                    .to_string(),
            ),
            Some(true) => PluginHealth::Ok,
        }
    }

    fn recheck(&self) -> bool {
        let ok = SlskdClient::new(self.effective_conn()).reachable();
        *self.reachable.lock().unwrap() = Some(ok);
        ok
    }

    fn save_state(&self) {
        persist::save(&self.cache_dir, &self.state.lock().unwrap());
    }

    fn set_data_dir(&self, dir: &str) -> Result<(), String> {
        let path = expand_home(dir);
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        *self.yaml_auth.lock().unwrap() = yaml_config::read(&path);
        self.state.lock().unwrap().data_dir = Some(dir.to_string());
        Ok(())
    }

    fn setup_existing(&self, dir: &str, user: &str, pass: &str, host: &str, log: &SetupLog) -> PluginHealth {
        log.say("Reading the slskd folder…");
        if let Err(e) = self.set_data_dir(dir) {
            return PluginHealth::Warn(e);
        }
        if !expand_home(dir).join("downloads").is_dir() {
            log.say("There is no downloads/ folder in there yet; playback needs it once slskd has downloaded something.");
        }
        if [host, user, pass].iter().any(|a| !a.is_empty()) {
            let mut conn = self.conn.lock().unwrap();
            if !host.is_empty() {
                conn.base_url = base_url(host);
            }
            if !user.is_empty() {
                conn.username = user.to_string();
            }
            if !pass.is_empty() {
                conn.password = pass.to_string();
            }
            self.state.lock().unwrap().connection =
                Some(persist::Connection { base_url: conn.base_url.clone(), username: conn.username.clone(), password: conn.password.clone() });
        }
        if self.state.lock().unwrap().docker.is_some() && docker::container_state().is_some() {
            log.say("Removing the slskd container medley made earlier; it would keep holding slskd's ports…");
            if let Err(e) = docker::remove() {
                return PluginHealth::Warn(format!("removing the old container failed: {e}"));
            }
        }
        self.state.lock().unwrap().docker = None;
        self.save_state();
        log.say(format!("Checking whether slskd answers at {}…", self.base_url()));
        self.recheck();
        self.health()
    }

    fn setup_docker(&self, folder_text: &str, soulseek_login: Option<(&str, &str)>, log: &SetupLog) -> PluginHealth {
        let warn = |m: String| PluginHealth::Warn(m);
        match docker::status() {
            docker::Status::Ready => {}
            docker::Status::NotInstalled => return warn("docker is not installed".to_string()),
            docker::Status::Unavailable(e) => return warn(format!("docker is not usable: {e}")),
        }
        let folder_text = if folder_text.is_empty() { DEFAULT_FOLDER } else { folder_text };
        let folder = folder_path(folder_text);
        let cache = &self.media_cache_dir;
        if let Err(e) = std::fs::create_dir_all(&folder).and_then(|()| std::fs::create_dir_all(cache)) {
            return warn(format!("cannot create {}: {e}", folder.display()));
        }
        let (Ok(folder), Ok(cache)) = (std::fs::canonicalize(&folder), std::fs::canonicalize(cache)) else {
            return warn(format!("cannot resolve {}", folder.display()));
        };
        if log.cancelled() {
            return warn("abandoned".to_string());
        }
        if !folder.join("slskd.yml").exists() {
            let Some((user, pass)) = soulseek_login else {
                return warn("no Soulseek login entered".to_string());
            };
            if let Err(e) = docker::write_config(&folder, user, pass) {
                return warn(e);
            }
        }
        if docker::container_state().is_some() {
            if self.state.lock().unwrap().docker.is_none() {
                return warn(format!(
                    "a container named {0} already exists and wasn't created by medley — remove it \
                     (docker rm -f {0}) or give the folder of that slskd instead",
                    docker::CONTAINER
                ));
            }
            if let Err(e) = docker::remove() {
                return warn(format!("removing the old container failed: {e}"));
            }
        }
        log.say("Starting slskd (the first start downloads it, which can take a few minutes)…");
        if let Err(e) = docker::create(&folder, &cache) {
            return warn(format!("docker run failed: {e}"));
        }

        let folder_str = folder.display().to_string();
        {
            let mut state = self.state.lock().unwrap();
            state.data_dir = Some(folder_str.clone());
            state.docker = Some(persist::Docker { folder: folder_str.clone(), mounted_cache: self.media_cache_dir.clone() });
            let mut conn = self.conn.lock().unwrap();
            conn.base_url = format!("http://127.0.0.1:{}", docker::HTTP_PORT);
            state.connection =
                Some(persist::Connection { base_url: conn.base_url.clone(), username: conn.username.clone(), password: conn.password.clone() });
        }
        *self.yaml_auth.lock().unwrap() = yaml_config::read(&folder);
        self.save_state();

        log.say(format!("Waiting for slskd to answer (up to {}s)…", STARTUP_WAIT.as_secs()));
        let deadline = Instant::now() + STARTUP_WAIT;
        while !self.recheck() && Instant::now() < deadline && !log.cancelled() {
            std::thread::sleep(Duration::from_secs(1));
        }
        *self.docker.lock().unwrap() = Some(snapshot_docker());
        if *self.reachable.lock().unwrap() == Some(true) {
            self.health()
        } else {
            warn(format!(
                "container started but slskd isn't answering at {} after {}s — see `docker logs {}`",
                self.base_url(),
                STARTUP_WAIT.as_secs(),
                docker::CONTAINER
            ))
        }
    }
}

fn snapshot_docker() -> DockerSnapshot {
    let status = docker::status();
    let container = (status == docker::Status::Ready).then(docker::container_state).flatten();
    DockerSnapshot { status, container }
}

fn folder_path(text: &str) -> PathBuf {
    let text = text.trim();
    expand_home(if text.is_empty() { DEFAULT_FOLDER } else { text })
}

/// `host`, `host:port` or a full URL, as the API base URL.
fn base_url(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    let with_scheme = if host.contains("://") { host.to_string() } else { format!("http://{host}") };
    let authority = with_scheme.split("://").nth(1).unwrap_or_default();
    if authority.contains(':') { with_scheme } else { format!("{with_scheme}:{}", docker::HTTP_PORT) }
}

impl Plugin for SoulseekPlugin {
    fn id(&self) -> SourceId {
        SourceId::from("soulseek")
    }

    fn probe(&self) -> PluginHealth {
        self.kick_off_connectivity_check();
        self.health()
    }

    fn setup_prompt(&self, answers: &[String]) -> Option<SetupPrompt> {
        if answers.len() == 1 {
            self.refresh_facts();
        }
        let base = self.base_url();
        let prompt = match self.plan(answers).get(answers.len())? {
            Step::Location if self.docker_offer() && self.state.lock().unwrap().docker.is_some() => SetupPrompt::new(
                "medley already set up slskd with Docker. Press Enter to recreate it (needed after the media cache moved), \
                 or type the path to the folder of an slskd you run yourself (the one with slskd.yml and downloads/) to use that instead.",
            ),
            Step::Location if self.docker_offer() => SetupPrompt::new(
                "Do you already run slskd, the Soulseek program medley searches and downloads through? If so, type the path to its \
                 folder (the one with slskd.yml and downloads/). If not, just press Enter and I will set it up for you with Docker.",
            ),
            Step::Location if *self.reachable.lock().unwrap() == Some(true) => SetupPrompt::new(format!(
                "slskd is answering at {base}. To play finished downloads, medley needs its folder (the one with slskd.yml and \
                 downloads/): type its path, or press Enter to leave it out (search works without it)."
            )),
            Step::Location if self.docker.lock().unwrap().is_none() => SetupPrompt::new(
                "Type the path to the folder of the slskd you run (the one with slskd.yml and downloads/). Docker is still being \
                 checked, so I cannot offer to set slskd up yet: reopen this in a few seconds for that, or press Enter to leave it for now.",
            ),
            Step::Location => SetupPrompt::new(
                "Type the path to the folder of the slskd you run (the one with slskd.yml and downloads/). Docker is not available \
                 here, so I cannot set slskd up for you; press Enter to leave it for now.",
            ),
            Step::Folder => SetupPrompt::new(
                "Where should slskd keep its settings? Any folder will do; it is created if it does not exist.",
            )
            .default(DEFAULT_FOLDER),
            Step::SoulseekUser => SetupPrompt::new(
                "Which Soulseek username should slskd use? A name nobody has taken yet registers a new free account.",
            ),
            Step::SoulseekPass => SetupPrompt::new("And its Soulseek password.").secret(),
            Step::Confirm => SetupPrompt::new(self.summary(answers)),
            Step::User => SetupPrompt::new("slskd's own web login has no username in its settings; which username does it use?")
                .default(self.conn.lock().unwrap().username.clone()),
            Step::Pass => SetupPrompt::new("And its password.").secret().default(self.conn.lock().unwrap().password.clone()),
            Step::Host => SetupPrompt::new(format!(
                "slskd does not answer at {base}. Press Enter if it is simply not running yet, or type where it is (host, host:port or URL)."
            ))
            .default(base),
        };
        Some(prompt)
    }

    fn setup_answer(&self, answers: &[String], answer: String) -> Result<String, String> {
        match self.plan(answers).get(answers.len()) {
            Some(Step::Location) if !answer.is_empty() => {
                let path = expand_home(&answer);
                match std::fs::canonicalize(&path) {
                    Ok(dir) if dir.is_dir() => Ok(dir.display().to_string()),
                    Ok(_) => Err(format!("{} is not a folder; type the path of slskd's folder.", path.display())),
                    Err(e) => Err(format!("{}: {e}", path.display())),
                }
            }
            Some(Step::SoulseekUser | Step::SoulseekPass) if answer.is_empty() => Err("slskd needs your Soulseek login; type it, or Esc to stop.".to_string()),
            _ => Ok(answer),
        }
    }

    fn detail(&self) -> Option<String> {
        let reach = match *self.reachable.lock().unwrap() {
            None => "checking".to_string(),
            Some(true) => format!("reachable at {}", self.base_url()),
            Some(false) => format!("not reachable at {}", self.base_url()),
        };
        let docker = match self.docker.lock().unwrap().as_ref() {
            None => "checking".to_string(),
            Some(DockerSnapshot { status: docker::Status::NotInstalled, .. }) => "not installed".to_string(),
            Some(DockerSnapshot { status: docker::Status::Unavailable(e), .. }) => format!("unusable ({e})"),
            Some(DockerSnapshot { container: Some(c), .. }) => format!("container {} {c}", docker::CONTAINER),
            Some(DockerSnapshot { container: None, .. }) => format!("no {} container", docker::CONTAINER),
        };
        Some(format!("slskd {reach}; docker {docker}"))
    }

    fn wiring(&self) -> Wiring {
        let client = SlskdClient::new(self.effective_conn());
        let state = self.state.lock().unwrap();
        let downloads: Option<PathBuf> = if state.docker.is_some() {
            Some(self.media_cache_dir.clone())
        } else {
            state.data_dir.as_deref().map(|d| expand_home(d).join("downloads"))
        };
        drop(state);
        let src = Arc::new(SoulseekSource::new(client, downloads));
        Wiring {
            source: Some(src.clone() as Arc<dyn Source>),
            media: Some(src as Arc<dyn MediaProvider>),
            player: None,
        }
    }

    fn setup(&self, answers: Vec<String>, log: &SetupLog) -> PluginHealth {
        let steps = self.plan(&answers);
        let get = |step: Step| steps.iter().position(|s| *s == step).and_then(|i| answers.get(i)).map_or("", String::as_str);
        if !get(Step::Location).is_empty() {
            return self.setup_existing(get(Step::Location), get(Step::User), get(Step::Pass), get(Step::Host), log);
        }
        if self.facts.lock().unwrap().docker_offer {
            log.say("Checking Docker…");
            let record = self.state.lock().unwrap().docker.clone();
            return match record {
                Some(d) => self.setup_docker(&d.folder, None, log),
                None => self.setup_docker(get(Step::Folder), Some((get(Step::SoulseekUser), get(Step::SoulseekPass))), log),
            };
        }
        log.say("Leaving slskd as it is.");
        self.recheck();
        self.health()
    }
}
