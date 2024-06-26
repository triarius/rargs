use lazy_static::lazy_static;
use regex::Regex;
use std::{
    borrow::Cow,
    cmp::max,
    collections::HashMap,
    convert::From,
    io::{self, BufRead},
    process::{Command, Stdio},
    sync::Arc,
};
use structopt::{clap::AppSettings, StructOpt};
use threadpool::ThreadPool;

const CONTEXT_KEY_LINENUM: &str = "LINENUM";
const CONTEXT_KEY_LINENUM_SHORT: &str = "LN";

fn main() {
    let mut exit_code = 0;

    let options = Options::from_args();
    let rargs = Arc::new(Rargs::new(&options));

    let stdin = io::stdin();

    let num_worker = if options.worker > 0 {
        options.worker
    } else {
        num_cpus::get()
    };
    let num_threads = if options.threads > 0 {
        options.threads
    } else {
        num_worker
    };

    let pool = ThreadPool::new(num_threads);

    let line_ending = if options.read0 { b'\0' } else { b'\n' };
    let mut line_num = options.startnum - 1;
    loop {
        let mut buffer = Vec::with_capacity(1024);
        match stdin.lock().read_until(line_ending, &mut buffer) {
            Ok(n) => {
                if n == 0 {
                    break;
                }

                // remove line-ending
                if buffer.ends_with(&[b'\r', b'\n']) {
                    buffer.pop();
                    buffer.pop();
                } else if buffer.ends_with(&[b'\n']) || buffer.ends_with(&[b'\0']) {
                    buffer.pop();
                }

                // execute command on line
                let rargs = rargs.clone();
                line_num += 1;
                let line = String::from_utf8(buffer).expect("Found invalid UTF8");

                if options.dryrun {
                    rargs.print_commands_to_be_executed(&line, line_num);
                } else {
                    pool.execute(move || {
                        rargs.execute_for_input(&line, line_num);
                    });
                }
            }
            Err(_err) => {
                // String not UTF8 or other error, skip.
                exit_code = 1;
                break;
            }
        }
    }

    pool.join();
    std::process::exit(exit_code);
}

lazy_static! {
    static ref CMD_REGEX: Regex = Regex::new(r"\{[[:space:]]*[^{}]*[[:space:]]*\}").unwrap();
    static ref FIELD_NAMED: Regex =
        Regex::new(r"^\{[[:space:]]*(?P<name>[[:word:]]*)[[:space:]]*\}$").unwrap();
    static ref FIELD_SINGLE: Regex =
        Regex::new(r"^\{[[:space:]]*(?P<num>-?\d+)[[:space:]]*\}$").unwrap();
    static ref FIELD_RANGE: Regex =
        Regex::new(r"^\{(?P<left>-?\d*)?\.\.(?P<right>-?\d*)?(?::(?P<sep>.*))?\}$").unwrap();
    static ref FIELD_SPLIT_RANGE: Regex =
        Regex::new(r"^\{(?P<left>-?\d*)?\.\.\.(?P<right>-?\d*)?\}$").unwrap();
}

#[derive(StructOpt, Debug)]
#[structopt(name = "Rargs", about = "Xargs with pattern matching")]
#[structopt(settings = &[AppSettings::TrailingVarArg])]
struct Options {
    #[structopt(
        long = "read0",
        short = "0",
        help = "Read input delimited by ASCII NUL(\\0) characters"
    )]
    read0: bool,

    #[structopt(
        long = "worker",
        short = "w",
        default_value = "1",
        help = "Deprecated. Number of threads to be used (same as --threads)"
    )]
    worker: usize,

    #[structopt(
        long = "threads",
        short = "j",
        default_value = "1",
        help = "Number of threads to be used"
    )]
    threads: usize,

    #[structopt(
        long = "pattern",
        short = "p",
        help = "regex pattern that captures the input"
    )]
    pattern: Option<String>,

    #[structopt(
        long = "separator",
        short = "s",
        default_value = " ",
        help = "separator for ranged fields"
    )]
    separator: String,

    #[structopt(
        long = "startnum",
        short = "n",
        default_value = "1",
        help = "start value for line number"
    )]
    startnum: i32,

    #[structopt(
        long = "delimiter",
        short = "d",
        conflicts_with = "pattern",
        help = "regex pattern used as delimiter (conflict with pattern)"
    )]
    delimiter: Option<String>,

    #[structopt(
        long = "dry-run",
        short = "e",
        help = "Print the commands to be executed without actually execute"
    )]
    dryrun: bool,

    #[structopt(required = true, help = "command to execute and its arguments")]
    cmd_and_args: Vec<String>,
}

#[derive(Debug)]
struct Rargs {
    pattern: Regex,
    command: String,
    args: Vec<ArgTemplate>,
    default_sep: String, // for output range fields
}

impl Rargs {
    pub fn new(opts: &Options) -> Self {
        let pattern;

        if let Some(pat_string) = opts.pattern.as_ref() {
            pattern = Regex::new(pat_string).unwrap();
        } else if let Some(delimiter) = opts.delimiter.as_ref() {
            let pat_string = format!(r"(.*?){}|(.*?)$", delimiter);
            pattern = Regex::new(&pat_string).unwrap();
        } else {
            pattern = Regex::new(r"(.*?)[[:space:]]+|(.*?)$").unwrap();
        }

        let command = opts.cmd_and_args[0].to_string();
        let args = opts.cmd_and_args[1..]
            .iter()
            .map(|s| ArgTemplate::from(&**s))
            .collect();
        let default_sep = opts.separator.clone();

        Rargs {
            pattern,
            command,
            args,
            default_sep,
        }
    }

    fn get_args(&self, line: &str, line_num: i32) -> Vec<String> {
        let context = RegexContext::builder(&self.pattern, line)
            .default_sep(Cow::Borrowed(&self.default_sep))
            .put(CONTEXT_KEY_LINENUM, Cow::Owned(line_num.to_string()))
            .put(CONTEXT_KEY_LINENUM_SHORT, Cow::Owned(line_num.to_string()))
            .build();

        self.args
            .iter()
            .flat_map(|arg| arg.apply_context(&context))
            .collect()
    }

    fn execute_for_input(&self, line: &str, line_num: i32) {
        let args = self.get_args(line, line_num);

        let status = Command::new(&self.command)
            .args(args)
            .stdin(Stdio::null())
            .status();

        if let Err(error) = status {
            eprintln!("rargs: {}: {}", self.command, error);
        }
    }

    fn print_commands_to_be_executed(&self, line: &str, line_num: i32) {
        let args = self.get_args(line, line_num);
        println!("{} {}", self.command, args.join(" "));
    }
}

trait Context<'a> {
    fn get_by_name(&'a self, group_name: &str) -> Option<Cow<'a, str>>;
    fn get_by_range(&'a self, range: &Range, sep: Option<&str>) -> Option<Cow<'a, str>>;
    fn get_by_split_range(&'a self, range: &Range) -> Vec<Cow<'a, str>>;
}

/// The context parsed from the input line using the pattern given. For Example:
///
/// input: 2018-10-21
/// pattern: "^(?P<year>\d{4})-(\d{2})-(\d{2})$"
///
/// will result in the context:
/// {}/{0} => "2018-10-21"
/// {1}/{year} => "2018"
/// {2} => "10"
/// {3} => "21"
struct RegexContext<'a> {
    map: HashMap<String, Cow<'a, str>>,
    groups: Vec<Cow<'a, str>>,
    default_sep: Cow<'a, str>,
}

impl<'a> RegexContext<'a> {
    fn builder(pattern: &'a Regex, content: &'a str) -> Self {
        let mut map = HashMap::new();
        map.insert("".to_string(), Cow::Borrowed(content));
        map.insert("0".to_string(), Cow::Borrowed(content));

        let group_names = pattern.capture_names().flatten().collect::<Vec<&str>>();

        let mut groups = vec![];

        for caps in pattern.captures_iter(content) {
            // the numbered group
            for mat in caps.iter().skip(1).flatten() {
                groups.push(Cow::Borrowed(mat.as_str()));
            }

            // the named group
            for name in group_names.iter() {
                if let Some(mat) = caps.name(name) {
                    map.insert(name.to_string(), Cow::Borrowed(mat.as_str()));
                }
            }
        }

        RegexContext {
            map,
            groups,
            default_sep: Cow::Borrowed(" "),
        }
    }

    pub fn default_sep(mut self, default_sep: Cow<'a, str>) -> Self {
        self.default_sep = default_sep;
        self
    }

    pub fn put(mut self, key: &str, value: Cow<'a, str>) -> Self {
        self.map.insert(key.to_string(), value);
        self
    }

    pub fn build(self) -> Self {
        self
    }

    fn translate_neg_index(&self, idx: i32) -> usize {
        let len = self.groups.len() as i32;
        let idx = if idx < 0 { idx + len + 1 } else { idx };
        max(0, idx) as usize
    }
}

impl<'a> Context<'a> for RegexContext<'a> {
    fn get_by_name(&'a self, group_name: &str) -> Option<Cow<'a, str>> {
        self.map.get(group_name).cloned()
    }

    fn get_by_range(&'a self, range: &Range, sep: Option<&str>) -> Option<Cow<'a, str>> {
        match *range {
            Single(num) => {
                let num = self.translate_neg_index(num);

                if num == 0 {
                    self.map.get("").cloned()
                } else if num > self.groups.len() {
                    None
                } else {
                    Some(self.groups[num - 1].clone())
                }
            }

            Both(left, right) => {
                let left = self.translate_neg_index(left);
                let right = self.translate_neg_index(right);

                if left == 0 {
                    return self.get_by_range(&LeftInf(right as i32), sep);
                } else if right > self.groups.len() {
                    return self.get_by_range(&RightInf(left as i32), sep);
                } else if left == right {
                    return self.get_by_range(&Single(left as i32), sep);
                }

                Some(Cow::Owned(
                    self.groups[(left - 1)..right].join(sep.unwrap_or(&self.default_sep)),
                ))
            }

            LeftInf(right) => {
                let right = self.translate_neg_index(right);
                if right > self.groups.len() {
                    return self.get_by_range(&Inf(), sep);
                }

                Some(Cow::Owned(
                    self.groups[..right].join(sep.unwrap_or(&self.default_sep)),
                ))
            }

            RightInf(left) => {
                let left = self.translate_neg_index(left);
                if left == 0 {
                    return self.get_by_range(&Inf(), sep);
                }

                Some(Cow::Owned(
                    self.groups[(left - 1)..].join(sep.unwrap_or(&self.default_sep)),
                ))
            }

            Inf() => Some(Cow::Owned(
                self.groups.join(sep.unwrap_or(&self.default_sep)),
            )),
        }
    }

    fn get_by_split_range(&'a self, range: &Range) -> Vec<Cow<'a, str>> {
        match *range {
            Single(num) => {
                let num = self.translate_neg_index(num);

                if num == 0 {
                    return self.map.get("").map_or_else(Vec::new, |c| vec![c.clone()]);
                } else if num > self.groups.len() {
                    return vec![];
                }

                vec![self.groups[num - 1].clone()]
            }

            Both(left, right) => {
                let left = self.translate_neg_index(left);
                let right = self.translate_neg_index(right);

                if left == 0 {
                    return self.get_by_split_range(&LeftInf(right as i32));
                } else if right > self.groups.len() {
                    return self.get_by_split_range(&RightInf(left as i32));
                } else if left == right {
                    return self.get_by_split_range(&Single(left as i32));
                }

                self.groups[(left - 1)..right].to_vec()
            }

            LeftInf(right) => {
                let right = self.translate_neg_index(right);
                if right > self.groups.len() {
                    return self.get_by_split_range(&Inf());
                }

                self.groups[..right].to_vec()
            }

            RightInf(left) => {
                let left = self.translate_neg_index(left);
                if left == 0 {
                    return self.get_by_split_range(&Inf());
                }

                self.groups[(left - 1)..].to_vec()
            }

            Inf() => self.groups.to_vec(),
        }
    }
}

#[derive(Clone, Debug)]
enum Range {
    Single(i32),
    Both(i32, i32),
    LeftInf(i32),
    RightInf(i32),
    Inf(),
}

use Range::*;

#[derive(Clone, Debug)]
enum ArgFragment {
    Literal(String),
    NamedGroup(String),
    RangeGroup(Range, Option<String>),
    SplitRangeGroup(Range),
}

use ArgFragment::*;

impl ArgFragment {
    fn parse(field_string: &str) -> Self {
        let opt_caps = FIELD_SINGLE.captures(field_string);
        if let Some(caps) = opt_caps {
            return RangeGroup(
                Single(
                    caps.name("num")
                        .expect("something is wrong in matching FIELD_SINGLE")
                        .as_str()
                        .parse()
                        .expect("field is not a number"),
                ),
                None,
            );
        }

        let opt_caps = FIELD_NAMED.captures(field_string);
        if let Some(caps) = opt_caps {
            return NamedGroup(
                caps.name("name")
                    .expect("something is wrong in matching FIELD_NAMED")
                    .as_str()
                    .to_string(),
            );
        }

        let opt_caps = FIELD_RANGE.captures(field_string);
        if let Some(caps) = opt_caps {
            let opt_left = caps.name("left").map(|s| s.as_str().parse().unwrap_or(1));
            let opt_right = caps.name("right").map(|s| s.as_str().parse().unwrap_or(-1));
            let opt_sep = caps.name("sep").map(|s| s.as_str().to_string());

            return match (opt_left, opt_right) {
                (None, None) => RangeGroup(Inf(), opt_sep),
                (None, Some(right)) => RangeGroup(LeftInf(right), opt_sep),
                (Some(left), None) => RangeGroup(RightInf(left), opt_sep),
                (Some(left), Some(right)) => RangeGroup(Both(left, right), opt_sep),
            };
        }

        let opt_caps = FIELD_SPLIT_RANGE.captures(field_string);
        if let Some(caps) = opt_caps {
            let opt_left = caps.name("left").map(|s| s.as_str().parse().unwrap_or(1));
            let opt_right = caps.name("right").map(|s| s.as_str().parse().unwrap_or(-1));

            return match (opt_left, opt_right) {
                (None, None) => SplitRangeGroup(Inf()),
                (None, Some(right)) => SplitRangeGroup(LeftInf(right)),
                (Some(left), None) => SplitRangeGroup(RightInf(left)),
                (Some(left), Some(right)) => SplitRangeGroup(Both(left, right)),
            };
        }

        Literal(field_string.to_string())
    }
}

/// The "compiled" template for arguments. for example:
///
/// "x {abc} z" will be compiled so that later `{abc}` could be replaced by actuals content
#[derive(Debug)]
struct ArgTemplate {
    fragments: Vec<ArgFragment>,
}

impl<'a> From<&'a str> for ArgTemplate {
    fn from(arg: &'a str) -> Self {
        let mut fragments = Vec::new();
        let mut last = 0;
        for mat in CMD_REGEX.find_iter(arg) {
            fragments.push(Literal(arg[last..mat.start()].to_string()));
            fragments.push(ArgFragment::parse(mat.as_str()));
            last = mat.end()
        }
        fragments.push(ArgFragment::Literal(arg[last..].to_string()));

        ArgTemplate { fragments }
    }
}

#[derive(Debug, Clone)]
enum Join {
    Literal(String),
    NamedGroup(String),
    RangeGroup(Range, Option<String>),
}

#[derive(Debug, Clone)]
struct Split(Range);

#[derive(Debug, Clone)]
enum Combination {
    Join(Vec<Join>),
    Split(Split),
}

impl<'a> ArgTemplate {
    fn apply_context<T: Context<'a>>(&self, context: &'a T) -> Vec<String> {
        let combinations = group_combinations(self.fragments.iter());
        combine_with_context(context, combinations.iter())
    }
}

/// Combine elements, splitting or joining the args as needed.
fn combine_with_context<'a, 'b, T: Context<'a>>(
    context: &'a T,
    combinations: impl Iterator<Item = &'b Combination>,
) -> Vec<String> {
    combinations
        .flat_map(|combination| match combination {
            Combination::Join(joins) => {
                let joined = joins
                    .iter()
                    .flat_map(|join| match join {
                        Join::Literal(ref literal) => vec![Cow::Borrowed(literal.as_str())],
                        Join::NamedGroup(ref name) => {
                            context.get_by_name(name).map_or_else(Vec::new, |c| vec![c])
                        }
                        Join::RangeGroup(ref range, ref opt_sep) => context
                            .get_by_range(range, opt_sep.as_ref().map(String::as_str))
                            .map_or_else(Vec::new, |c| vec![c]),
                    })
                    .collect::<String>();
                vec![joined]
            }
            Combination::Split(Split(ref range)) => context
                .get_by_split_range(range)
                .iter()
                .map(|s| s.as_ref().to_owned())
                .collect::<Vec<String>>(),
        })
        .collect()
}

/// Group the args by whether they should be split or joined in the output
fn group_combinations<'a>(fragments: impl Iterator<Item = &'a ArgFragment>) -> Vec<Combination> {
    fragments.fold(vec![], |mut acc: Vec<Combination>, e| {
        let mut tail = match acc.pop() {
            None => match e {
                Literal(s) => {
                    if s.is_empty() {
                        vec![]
                    } else {
                        vec![Combination::Join(vec![Join::Literal(s.clone())])]
                    }
                }
                NamedGroup(s) => vec![Combination::Join(vec![Join::NamedGroup(s.clone())])],
                RangeGroup(r, s) => vec![Combination::Join(vec![Join::RangeGroup(
                    r.clone(),
                    s.clone(),
                )])],
                SplitRangeGroup(r) => vec![Combination::Split(Split(r.clone()))],
            },
            Some(last) => match (last, e) {
                (last, SplitRangeGroup(r)) => {
                    vec![last, Combination::Split(Split(r.clone()))]
                }
                (Combination::Join(mut joins), Literal(s)) => {
                    joins.push(Join::Literal(s.clone()));
                    vec![Combination::Join(joins)]
                }
                (Combination::Join(mut joins), NamedGroup(s)) => {
                    joins.push(Join::NamedGroup(s.clone()));
                    vec![Combination::Join(joins)]
                }
                (Combination::Join(mut joins), RangeGroup(r, s)) => {
                    joins.push(Join::RangeGroup(r.clone(), s.clone()));
                    vec![Combination::Join(joins)]
                }
                (last, Literal(s)) => {
                    if s.is_empty() {
                        vec![last]
                    } else {
                        vec![last, Combination::Join(vec![Join::Literal(s.clone())])]
                    }
                }
                (last, NamedGroup(s)) => {
                    vec![last, Combination::Join(vec![Join::NamedGroup(s.clone())])]
                }
                (last, RangeGroup(r, s)) => {
                    vec![
                        last,
                        Combination::Join(vec![Join::RangeGroup(r.clone(), s.clone())]),
                    ]
                }
            },
        };

        acc.append(&mut tail);

        acc
    })
}
