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
    /// Open the manual (fallback dump) for a seek dir in the PDF viewer
    Open {
        /// Seek dir (default: most recent under ~/.cache/magpie/seek/)
        dir: Option<String>,
    },
    /// Judge a finished manual: fact-check top claims against fresh
    /// searches, text verdict; APPROVED appends a verdict appendix
    Judge {
        /// Seek dir (default: most recent under ~/.cache/magpie/seek/)
        dir: Option<String>,
    },
    /// Pixie pass: one quick local AI task per crawled entry, then structured PDF
    Process {
        /// Seek dir (contains seek.json)
        dir: String,
    },
    /// End-to-end manual: seek (raw dump) + pixie pass (structured manual.pdf)
    Manual {
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
    },
    /// Smell the hoard: BM25 search over a subject's Xapian nose
    /// (needs the `nose` cargo feature; rebuilds from seek dirs)
    #[cfg(feature = "nose")]
    Nose {        /// Search query (omit with --rebuild)
        query: Option<String>,
        /// Re-press the index from seek dirs instead of searching
        #[arg(long, default_value_t = false)]
        rebuild: bool,
        /// Subject slug (default: latest seek's query)
        #[arg(long)]
        subject: Option<String>,
        /// Index these seek dirs (repeatable; default: all cache dirs
        /// matching the subject). Custom --out runs live outside the cache.
        #[arg(long)]
        dir: Vec<String>,
        /// Query language for stemming (ISO code, default en)
        #[arg(long, default_value = "en")]
        lang: String,
        /// Max hits
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Rinse the hoard: press runs into the Parquet granary, query it
    /// with SQL (needs the `granary` cargo feature)
    #[cfg(feature = "granary")]
    Granary {
        /// SQL to run over the granary (omit with --rebuild)
        #[arg(long)]
        sql: Option<String>,
        /// Press seek dirs into parquet instead of querying
        #[arg(long, default_value_t = false)]
        rebuild: bool,
        /// Press these seek dirs (repeatable; default: all cache dirs)
        #[arg(long)]
        dir: Vec<String>,
        /// Max rows displayed
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}
