// PACKET PROCESSING ENGINE
//
// This module implements configuration and execution of the packet processing
// engine.
//
//   EngineStats - struct containing global engine statistics
//   stats() -> EngineStats - get engine statistics
//   EngineState - struct representing engine state
//   state() -> &'static EngineState - get engine state
//   SharedLink - type for shared links (between apps, also in EngineState)
//   AppState - struct representing an app in the current app network
//   App, AppConfig - traits that defines an app, and its configuration
//   PULL_NPACKETS - number of packets to be inhaled in app’s pull() methods
//   configure(&mut EngineState, &config) - apply configuration to app network
//   main(&EngineState, Options) - run the engine breathe loop
//   Options - engine breathe loop options
//   now() -> Instant - return current monotonic engine time
//   timeout(Duration) -> [()->bool] - make timer returning true after duration
//   report_load() - print load report
//   report_links() - print link statistics

use super::config;
use super::lib;
use super::link;

use once_cell::unsync::Lazy;
use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const MAXSLEEP: u64 = 100;

struct Engine {
    stats: EngineStats,
    state: EngineState,
    // Return current monotonic time.
    // Can be used to drive timers in apps.
    monotonic_now: Option<Instant>,
    lastfrees: u64,
    sleep: u64,
    lastloadreport: Option<Instant>,
    reportedfrees: u64,
    reportedfreebits: u64,
    reportedfreebytes: u64,
    reportedbreaths: u64,
}

impl Engine {
    // @TODO impl singleton
    fn new() -> Self {
        Engine {
            stats: EngineStats::new(),
            state: EngineState::new(),
            monotonic_now: None, // original intent?
            lastfrees: 0,
            sleep: 0,
            lastloadreport: None,
            reportedfrees: 0,
            reportedfreebits: 0,
            reportedfreebytes: 0,
            reportedbreaths: 0,
        }
    }

    // Call this to “run snabb”.
    pub fn main(&mut self, options: Option<Options>) {
        let options = match options {
            Some(options) => options,
            None => Options {
                ..Default::default()
            },
        };
        let mut done = options.done;
        if let Some(duration) = options.duration {
            assert!(
                done.is_none(),
                "You can not have both 'duration' and 'done'"
            );
            done = Some(self.timeout(duration));
        }

        self.breathe();
        while match &done {
            Some(done) => !done(),
            None => true,
        } {
            self.pace_breathing();
            self.breathe();
        }
        if !options.no_report {
            if options.report_load {
                self.report_load();
            }
            if options.report_links {
                self.report_links();
            }
            if options.report_apps {
                self.report_apps();
            }
        }

        self.monotonic_now = None;
    }

    // Load reporting prints several metrics:
    //   time  - period of time that the metrics were collected over
    //   fps   - frees per second (how many calls to packet::free())
    //   fpb   - frees per breath
    //   bpp   - bytes per packet (average packet size)
    //   sleep - usecs of sleep between breaths
    pub fn report_load(&self) {
        let frees = self.stats.frees;
        let freebits = self.stats.freebits;
        let freebytes = self.stats.freebytes;
        let breaths = self.stats.breaths;
        if let Some(lastloadreport) = self.lastloadreport {
            let interval = self.now().duration_since(lastloadreport).as_secs_f64();
            let newfrees = frees - self.reportedfrees;
            let newbits = freebits - self.reportedfreebits;
            let newbytes = freebytes - self.reportedfreebytes;
            let newbreaths = breaths - self.reportedbreaths;
            let fps = (newfrees as f64 / interval) as u64;
            let fbps = newbits as f64 / interval;
            let fpb = if newbreaths > 0 {
                newfrees / newbreaths
            } else {
                0
            };
            let bpp = if newfrees > 0 { newbytes / newfrees } else { 0 };
            println!(
                "load: time: {:.2} fps: {} fpGbps: {:.3} fpb: {} bpp: {} sleep: {}",
                interval,
                lib::comma_value(fps),
                fbps / 1e9,
                lib::comma_value(fpb),
                lib::comma_value(bpp),
                self.sleep
            );
        }
        self.lastloadreport = Some(self.now());
        self.reportedfrees = frees;
        self.reportedfreebits = freebits;
        self.reportedfreebytes = freebytes;
        self.reportedbreaths = breaths;
    }

    // Breathing regluation to reduce CPU usage when idle by calling sleep.
    //
    // Dynamic adjustment automatically scales the time to sleep between
    // breaths from nothing up to MAXSLEEP (default: 100us). If packets
    // are processed during a breath then the SLEEP period is halved, and
    // if no packets are processed during a breath then the SLEEP interval
    // is increased by one microsecond.
    fn pace_breathing(&mut self) {
        unsafe {
            if self.lastfrees == self.stats.frees {
                self.sleep = min(self.sleep + 1, MAXSLEEP);
                sleep(Duration::from_micros(self.sleep));
            } else {
                self.sleep /= 2;
            }
            self.lastfrees = self.stats.frees;
        }
    }

    // Make a closure which when called returns true after duration,
    // and false otherwise.
    pub fn timeout(&self, duration: Duration) -> Box<dyn Fn() -> bool> {
        let deadline = self.now() + duration;
        Box::new(move || Instant::now() > deadline)
    }

    // Return a throttle function.
    //
    // The throttle returns true at most once in any <duration> time interval.
    pub fn throttle(&self, duration: Duration) -> Box<dyn FnMut() -> bool> {
        let mut deadline = self.now();
        Box::new(move || {
            if Instant::now() > deadline {
                deadline = Instant::now() + duration;
                true
            } else {
                false
            }
        })
    }

    // Perform a single breath (inhale / exhale)
    fn breathe(&mut self) {
        self.monotonic_now = Some(Instant::now());
        for name in self.state.inhale {
            let app = self.state.app_table.get(&*name).unwrap();
            app.app.pull(&app);
        }
        for name in self.state.exhale {
            let app = self.state.app_table.get(&*name).unwrap();
            app.app.push(&app);
        }
        self.stats.breaths += 1;
    }

    pub fn now(&self) -> Instant {
        match self.monotonic_now {
            Some(instant) => instant,
            None => Instant::now(),
        }
    }

    pub fn add_frees(&mut self) {
        self.stats.frees += 1;
    }

    pub fn add_freebytes(&mut self, bytes: u64) {
        self.stats.freebytes += bytes;
    }

    pub fn add_freebits(&mut self, bits: u64) {
        self.stats.freebits += bits;
    }

    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    // Configure the running app network to match (new) config.
    //
    // Successive calls to configure() will migrate from the old to the
    // new app network by making the changes needed.
    pub fn configure(&mut self, config: &config::Config) {
        // First determine the links that are going away and remove them.
        for link in self.state.link_table.clone().keys() {
            if config.links.get(link).is_none() {
                self.state.unlink_apps(link)
            }
        }
        // Do the same for apps.
        let apps: Vec<_> = self.state.app_table.keys().map(Clone::clone).collect();
        for name in apps {
            let old = &self.state.app_table.get(&name).unwrap().conf;
            match config.apps.get(&name) {
                Some(new) => {
                    if !old.equal(&**new) {
                        self.state.stop_app(&name)
                    }
                }
                None => self.state.stop_app(&name),
            }
        }
        // Start new apps.
        for (name, app) in config.apps.iter() {
            if self.state.app_table.get(name).is_none() {
                self.state.start_app(name, &**app)
            }
        }
        // Rebuild links.
        for link in config.links.iter() {
            self.state.link_apps(link);
        }
        // Compute breathe order.
        self.state.compute_breathe_order();
    }

    // Print a link report (packets sent, percent dropped)
    pub fn report_links(&self) {
        println!("Link report:");
        let mut names: Vec<_> = self.state.link_table.keys().collect();
        names.sort();
        for name in names {
            let link = self.state.link_table.get(name).unwrap().borrow();
            let txpackets = link.txpackets;
            let txdrop = link.txdrop;
            println!(
                "  {} sent on {} (loss rate: {}%)",
                lib::comma_value(txpackets),
                name,
                loss_rate(txdrop, txpackets)
            );
        }
    }

    // Print a report of all active apps
    pub fn report_apps(&self) {
        for (name, app) in self.state.app_table.iter() {
            println!("App report for {}:", name);
            match app.input.len() {
                0 => (),
                1 => println!("  receiving from one input link"),
                n => println!("  receiving from {} input links", n),
            }
            match app.output.len() {
                0 => (),
                1 => println!("  transmitting to one output link"),
                n => println!("  transmitting to {} output links", n),
            }
            if app.app.has_report() {
                app.app.report();
            }
        }
    }
}

// Counters for global engine statistics.
#[derive(Default)]
pub struct EngineStats {
    pub breaths: u64,   // Total breaths taken
    pub frees: u64,     // Total packets freed
    pub freebits: u64,  // Total packet bits freed (for 10GbE)
    pub freebytes: u64, // Total packet bytes freed
}

impl EngineStats {
    fn new() -> Self {
        EngineStats::default()
    }
}

// Global engine state; singleton obtained via engine::init()
//
// The set of all active apps and links in the system, indexed by name.
pub struct EngineState {
    pub link_table: HashMap<String, SharedLink>,
    pub app_table: HashMap<String, AppState>,
    pub inhale: Vec<String>,
    pub exhale: Vec<String>,
}

impl EngineState {
    fn new() -> Self {
        EngineState {
            app_table: HashMap::new(),
            link_table: HashMap::new(),
            inhale: Vec::new(),
            exhale: Vec::new(),
        }
    }

    // Remove link between two apps.
    fn unlink_apps(&mut self, spec: &str) {
        self.link_table.remove(spec);
        let spec = config::parse_link(spec);
        self.app_table
            .get_mut(&spec.from)
            .unwrap()
            .output
            .remove(&spec.output);
        self.app_table
            .get_mut(&spec.to)
            .unwrap()
            .input
            .remove(&spec.input);
    }

    // Insert new app instance into network.
    fn start_app(&mut self, name: &str, conf: &dyn AppArg) {
        let conf = conf.box_clone();
        self.app_table.insert(
            name.to_string(),
            AppState {
                app: conf.new(),
                conf,
                input: HashMap::new(),
                output: HashMap::new(),
            },
        );
    }

    // Remove app instance from network.
    fn stop_app(&mut self, name: &str) {
        let removed = self.app_table.remove(name).unwrap();
        if removed.app.has_stop() {
            removed.app.stop();
        }
    }

    // Link two apps in the network.
    fn link_apps(&mut self, spec: &str) {
        let link = self
            .link_table
            .entry(spec.to_string())
            .or_insert_with(new_shared_link);
        let spec = config::parse_link(spec);
        self.app_table
            .get_mut(&spec.from)
            .unwrap()
            .output
            .insert(spec.output, link.clone());
        self.app_table
            .get_mut(&spec.to)
            .unwrap()
            .input
            .insert(spec.input, link.clone());
    }

    // Compute engine breathe order
    //
    // Ensures that the order in which pull/push callbacks are processed in
    // breathe()...
    //   - follows link dependencies when possible (to optimize for latency)
    //   - executes each app’s callbacks at most once (cycles imply that some
    //     packets may remain on links after breathe() returns)
    //   - is deterministic with regard to the configuration
    fn compute_breathe_order(&mut self) {
        self.inhale.clear();
        self.exhale.clear();
        // Build map of successors
        let mut successors: HashMap<String, HashSet<String>> = HashMap::new();
        for link in self.link_table.keys() {
            let spec = config::parse_link(&link);
            successors
                .entry(spec.from)
                .or_insert_with(HashSet::new)
                .insert(spec.to);
        }
        // Put pull apps in inhalers
        for (name, app) in self.app_table.iter() {
            if app.app.has_pull() {
                self.inhale.push(name.to_string());
            }
        }
        // Sort inhalers by name (to ensure breathe order determinism)
        self.inhale.sort();
        // Collect initial dependents
        let mut dependents = Vec::new();
        for name in &self.inhale {
            if let Some(successors) = successors.get(name) {
                for successor in successors.iter() {
                    let app = self.app_table.get(successor).unwrap();
                    if app.app.has_push() && !dependents.contains(successor) {
                        dependents.push(successor.to_string());
                    }
                }
            }
        }
        // Remove processed successors (resolved dependencies)
        for name in &self.inhale {
            successors.remove(name);
        }
        // Compute sorted push order
        while !dependents.is_empty() {
            // Attempt to delay dependents after their inputs, but break cycles by
            // selecting at least one dependent.
            let mut selected = HashSet::new();
            for name in dependents.clone() {
                if let Some(successors) = successors.get(&name) {
                    for successor in successors.iter() {
                        if !selected.contains(successor)
                            && dependents.contains(successor)
                            && dependents.len() > 1
                        {
                            selected.insert(name.to_string());
                            dependents.retain(|name| name != successor);
                        }
                    }
                }
            }
            // Sort dependents by name (to ensure breathe order determinism)
            dependents.sort();
            // Drain and append dependents to exhalers
            let exhaled = dependents.clone();
            self.exhale.append(&mut dependents);
            // Collect further dependents
            for name in &exhaled {
                if let Some(successors) = successors.get(name) {
                    for successor in successors.iter() {
                        let app = self.app_table.get(successor).unwrap();
                        if app.app.has_push()
                            && !self.exhale.contains(successor)
                            && !dependents.contains(successor)
                        {
                            dependents.push(successor.to_string());
                        }
                    }
                }
            }
            // Remove processed successors (resolved dependencies)
            for name in &exhaled {
                successors.remove(name);
            }
        }
    }
}

// Type for links shared between apps.
//
// Links are borrowed at runtime by apps to perform packet I/O, or via the
// global engine state (to query link statistics etc.)
pub type SharedLink = Rc<RefCell<link::Link>>;

// State for a sigle app instance managed by the engine
//
// Tracks a reference to the AppConfig used to instantiate the app, and maps of
// its active input and output links.
pub struct AppState {
    pub app: Box<dyn App>,
    pub conf: Box<dyn AppArg>,
    pub input: HashMap<String, SharedLink>,
    pub output: HashMap<String, SharedLink>,
}

// Callbacks that can be implented by apps
//
//   pull: inhale packets into the app network (put them onto output links)
//   push: exhale packets out the the app network (move them from input links
//         to output links, or peripheral device queues)
//   stop: stop the app (deinitialize)
pub trait App {
    fn has_pull(&self) -> bool {
        false
    }
    fn pull(&self, _app: &AppState) {
        unimplemented!();
    }
    fn has_push(&self) -> bool {
        false
    }
    fn push(&self, _app: &AppState) {
        unimplemented!();
    }
    fn has_report(&self) -> bool {
        false
    }
    fn report(&self) {
        unimplemented!();
    }
    fn has_stop(&self) -> bool {
        false
    }
    fn stop(&self) {
        unimplemented!();
    }
}
// Recommended number of packets to inhale in pull()
pub const PULL_NPACKETS: usize = link::LINK_MAX_PACKETS / 10;

// Constructor trait/callback for app instance specifications
//
//   new: initialize and return app (resulting app must implement App trait)
//
// Objects that implement the AppConfig trait can be used to configure apps
// via config::app().
pub trait AppConfig: std::fmt::Debug {
    fn new(&self) -> Box<dyn App>;
}

// Trait used internally by engine/config to provide an equality predicate for
// implementors of AppConfig. Sort of a hack based on the Debug trait.
//
// Auto-implemented for all implementors of AppConfig.
pub trait AppArg: AppConfig + AppClone {
    fn identity(&self) -> String {
        format!("{}::{:?}", module_path!(), self)
    }
    fn equal(&self, y: &dyn AppArg) -> bool {
        self.identity() == y.identity()
    }
}
impl<T: AppConfig + AppClone> AppArg for T {}

// We need to be able to copy (clone) AppConfig objects from configurations
// into the engine state. However, the Rust compiler does not allow
// AppConfig/AppArg to implement Clone(/Sized) if we want to use them for trait
// objects.
//
// The AppClone trait below (which we can bind AppArg to) auto-implements a
// box_clone[1] method for all implementors of AppConfig as per
// https://users.rust-lang.org/t/solved-is-it-possible-to-clone-a-boxed-trait-object/1714/6
pub trait AppClone: AppConfig {
    fn box_clone(&self) -> Box<dyn AppArg>;
}
impl<T: AppConfig + Clone + 'static> AppClone for T {
    fn box_clone(&self) -> Box<dyn AppArg> {
        Box::new((*self).clone())
    }
}
impl Clone for Box<dyn AppArg> {
    fn clone(&self) -> Self {
        (*self).box_clone()
    }
}

// Allocate a fresh shared link.
fn new_shared_link() -> SharedLink {
    Rc::new(RefCell::new(link::new()))
}

// Engine breathe loop Options
//
//  done: run the engine until predicate returns true
//  duration: run the engine for duration (mutually exclusive with 'done')
//  no_report: disable engine reporting before return
//  report_load: print a load report upon return
//  report_links: print summarized statistics for each link upon return
//  report_apps: print app defined report for each app
#[derive(Default)]
pub struct Options {
    pub done: Option<Box<dyn Fn() -> bool>>,
    pub duration: Option<Duration>,
    pub no_report: bool,
    pub report_load: bool,
    pub report_links: bool,
    pub report_apps: bool,
}

fn loss_rate(drop: u64, sent: u64) -> u64 {
    if sent == 0 {
        return 0;
    }
    drop * 100 / (drop + sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basic_apps;
    use crate::config;

    #[test]
    fn engine() {
        let mut c = config::new();
        config::app(&mut c, "source", &basic_apps::Source { size: 60 });
        config::app(&mut c, "sink", &basic_apps::Sink {});
        config::link(&mut c, "source.output -> sink.input");
        configure(&c);
        println!("Configured the app network: source(60).output -> sink.input");
        main(Some(Options {
            duration: Some(Duration::new(0, 0)),
            report_load: true,
            report_links: true,
            ..Default::default()
        }));
        let mut c = c.clone();
        config::app(&mut c, "source", &basic_apps::Source { size: 120 });
        configure(&c);
        println!("Cloned, mutated, and applied new configuration:");
        println!("source(120).output -> sink.input");
        main(Some(Options {
            done: Some(Box::new(|| true)),
            report_load: true,
            report_links: true,
            ..Default::default()
        }));
        let stats = stats();
        println!(
            "engine: frees={} freebytes={} freebits={}",
            stats.frees, stats.freebytes, stats.freebits
        );
    }

    #[test]
    fn breathe_order() {
        println!("Case 1:");
        let mut c = config::new();
        config::app(&mut c, "a_io1", &PseudoIO {});
        config::app(&mut c, "b_t1", &basic_apps::Tee {});
        config::app(&mut c, "c_t2", &basic_apps::Tee {});
        config::app(&mut c, "d_t3", &basic_apps::Tee {});
        config::link(&mut c, "a_io1.output -> b_t1.input");
        config::link(&mut c, "b_t1.output -> c_t2.input");
        config::link(&mut c, "b_t1.output2 -> d_t3.input");
        config::link(&mut c, "d_t3.output -> b_t1.input2");
        configure(&c);
        report_links();
        for name in &state().inhale {
            println!("pull {}", &name);
        }
        for name in &state().exhale {
            println!("push {}", &name);
        }
        println!("Case 2:");
        let mut c = config::new();
        config::app(&mut c, "a_io1", &PseudoIO {});
        config::app(&mut c, "b_t1", &basic_apps::Tee {});
        config::app(&mut c, "c_t2", &basic_apps::Tee {});
        config::app(&mut c, "d_t3", &basic_apps::Tee {});
        config::link(&mut c, "a_io1.output -> b_t1.input");
        config::link(&mut c, "b_t1.output -> c_t2.input");
        config::link(&mut c, "b_t1.output2 -> d_t3.input");
        config::link(&mut c, "c_t2.output -> d_t3.input2");
        configure(&c);
        report_links();
        for name in &state().inhale {
            println!("pull {}", &name);
        }
        for name in &state().exhale {
            println!("push {}", &name);
        }
        println!("Case 3:");
        let mut c = config::new();
        config::app(&mut c, "a_io1", &PseudoIO {});
        config::app(&mut c, "b_t1", &basic_apps::Tee {});
        config::app(&mut c, "c_t2", &basic_apps::Tee {});
        config::link(&mut c, "a_io1.output -> b_t1.input");
        config::link(&mut c, "a_io1.output2 -> c_t2.input");
        config::link(&mut c, "b_t1.output -> a_io1.input");
        config::link(&mut c, "b_t1.output2 -> c_t2.input2");
        config::link(&mut c, "c_t2.output -> a_io1.input2");
        configure(&c);
        report_links();
        for name in &state().inhale {
            println!("pull {}", &name);
        }
        for name in &state().exhale {
            println!("push {}", &name);
        }
    }

    #[derive(Clone, Debug)]
    pub struct PseudoIO {}
    impl AppConfig for PseudoIO {
        fn new(&self) -> Box<dyn App> {
            Box::new(PseudoIOApp {})
        }
    }
    pub struct PseudoIOApp {}
    impl App for PseudoIOApp {
        fn has_pull(&self) -> bool {
            true
        }
        fn has_push(&self) -> bool {
            true
        }
    }
}
