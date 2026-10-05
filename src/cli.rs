use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "magpie", version, about = "thorough seeker — TUI prompt -> crawl -> PDF")]
pub struct Cli {
    /// Query when called as `magpie \"some question\"` (one-shot seek)
    #[arg(default_value = None)]
    pub query: Option<String>,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// Force TUI even with query
    #[arg(long, global = true)]
    pub tui: bool,

    /// Fetch top pages then exit (compat with python magpie -a)
    #[arg(short = 'a', long, global = true)]
    pub deep: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Cmd {
    /// Thorough seek: crawl web carefully, save JSON + PDF
    Seek {
        /// What to seek
        query: String,
        /// Output dir (default ~/.cache/magpie/seek/<slug>/)
        #[arg(long)]
        out: Option<String>,
        /// Depth: quick|deep|extreme
        #[arg(long, default_value = "deep")]
        depth: String,
        /// Max pages to fetch
        #[arg(long, default_value_t = 12)]
        pages: usize,
        /// Skip PDF, JSON only
        #[arg(long, default_value_t = false)]
        no_pdf: bool,
    },
    /// Compat: dump page text (for pixie `magpie browse --dump URL`)
    Browse {
        #[arg(default_value = "")]
        rest: String,
        #[arg(long, default_value_t = false)]
        dump: bool,
    },
}
