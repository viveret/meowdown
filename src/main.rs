use std::{
    cell::RefCell, collections::HashMap, error::Error, fs::{self, File}, io::{Read, Write}, path::{Path, PathBuf}, process::Command, rc::Rc, time::{Duration, SystemTime}
};

use chrono::{DateTime, Local, Utc};
use clap::Parser;
use notify::{RecommendedWatcher, Watcher};
use pulldown_cmark::{html, Event, Options, Tag};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;


/**
 * Features:
 * - make static websites
 * - HTML templates, markdown to HTML, copy assets to output
 * - control block tags, helper functions
 * - build, clean, new project
 * - Live reload / watch and rebuild
 * - List of posts / markdown entries as a view
 */


/**
 * TODO / Bug Fixes:
 * - more fun things (interactive or social)
 * - different styles and style pallet
 * - more warnings and error messages
 * - break up project into different files
 * - scripting with something like lua?
 * - page tags and categories
 * - index to view all pages with tags/categories
 * - verbosity to output levels for debugging without rebuilding/changing code, avoid too much output
 */


// ========== Data Structures ==========

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub config_path: Option<String>,
    pub input_dir: String,
    pub output_dir: String,
    pub watch: bool,
    pub variant: Option<String>,
    pub variants: Option<Vec<String>>,
    pub generate_robots_txt: Option<bool>,
    pub generate_sitemap_xml: Option<bool>,
}

type FrontMatter = HashMap<String, String>;
type TemplateContextPtr = Rc<RefCell<TemplateContext>>;
type TemplateFunc = dyn Fn(&[String], Option<&str>, TemplateContextPtr, &mut GlobalContext) -> String + 'static;
type TemplateFuncPtr = Rc<TemplateFunc>;

#[derive(Debug)]
struct TemplateContext {
    strings: HashMap<String, String>,
    nodes: HashMap<String, Rc<TemplateNode>>,
    json_data: HashMap<String, Value>,
    parent: Option<TemplateContextPtr>,
}

#[derive(Debug)]
enum TemplateNode {
    Page {
        path: String,
        front_matter: FrontMatter,
        content_node: Rc<TemplateNode>,
        output_path: PathBuf,
        parent: Option<Rc<TemplateNode>>,
    },
    Layout {
        name: String,
        front_matter: FrontMatter,
        content_node: Rc<TemplateNode>,
        parent: Option<Rc<TemplateNode>>,
    },
    IfBlock {
        condition: String,
        true_branch: Rc<TemplateNode>,
        false_branch: Option<Rc<TemplateNode>>,
    },
    ForEachBlock {
        key: String,
        item_name: String,
        body: Rc<TemplateNode>,
    },
    Func {
        name: String,
        args: Vec<String>,
        block_content: Option<String>,
    },
    StringContent(String),
    Composite(Vec<TemplateNode>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RobotsConfig {
    // Global crawl delay in seconds
    pub crawl_delay: Option<u32>,
    
    // Sitemap location (relative to site root)
    pub sitemap: Option<String>,
    
    // User-agent specific rules (supports multiple agents per rule)
    pub user_agents: Option<Vec<RobotsUserAgentRules>>,
    
    // Global allow/disallow rules that apply to all agents
    pub global_rules: Option<RobotsGlobalRules>,

    // Auto-disallow any HTML files not marked for inclusion
    pub auto_disallow_non_included_html: Option<bool>,

    // Auto-include generated HTML files
    pub auto_include_generated_html: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RobotsUserAgentRules {
    // Multiple user agents these rules apply to
    pub user_agents: Vec<String>,
    
    // Paths to allow for these agents
    pub allow: Option<Vec<String>>,
    
    // Paths to disallow for these agents
    pub disallow: Option<Vec<String>>,
    
    // Crawl delay for these agents
    pub crawl_delay: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RobotsGlobalRules {
    // Paths to allow (relative to site root)
    pub allow: Option<Vec<String>>,
    
    // Paths to disallow (relative to site root)
    pub disallow: Option<Vec<String>>,
}

// Represents a single entry in a sitemap.xml file
#[derive(Debug, Clone)]
pub struct SitemapXmlNode {
    // The URL location (required)
    pub loc: String,
    
    // Last modification date (optional)
    pub lastmod: Option<DateTime<Utc>>,
    
    // Change frequency (optional)
    pub changefreq: Option<ChangeFrequency>,
    
    // Priority (0.0 to 1.0, optional)
    pub priority: Option<f32>,
    
    // Alternate language versions (optional)
    pub alternates: Vec<AlternateLink>,
}

// Frequency of page changes
#[derive(Debug, Clone, strum::Display, strum::EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum ChangeFrequency {
    Always,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Never,
}

// Alternate language/location version
#[derive(Debug, Clone)]
pub struct AlternateLink {
    // URL of alternate version
    pub url: String,
    
    // Language code (e.g., "en", "fr")
    pub lang: String,
}

struct GlobalContext {
    cfg: Config,
    layout_cache: HashMap<String, Rc<TemplateNode>>,
    site_strings: HashMap<String, String>,
    functions: HashMap<String, TemplateFuncPtr>,
}

// ========== Struct Implementations ====

impl TemplateContext {
    pub fn new(parent: Option<TemplateContextPtr>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            strings: HashMap::new(),
            nodes: HashMap::new(),
            json_data: HashMap::new(),
            parent,
        }))
    }
    
    pub fn add_front_matter(&mut self, front_matter: &FrontMatter) {
        self.strings.extend(front_matter.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.strings.get(key).cloned()
            .or_else(|| self.parent.as_ref()?.borrow().get_string(key))
    }
}

impl TemplateNode {
    pub fn new_page(
        path: String,
        front_matter: FrontMatter,
        content_node: Rc<TemplateNode>,
        output_path: PathBuf,
        parent: Option<Rc<TemplateNode>>,
    ) -> Rc<Self> {
        Rc::new(Self::Page {
            path,
            front_matter,
            content_node,
            output_path,
            parent,
        })
    }
    
    pub fn new_layout(
        name: String,
        front_matter: FrontMatter,
        content_node: Rc<TemplateNode>,
        parent: Option<Rc<TemplateNode>>,
    ) -> Rc<Self> {
        Rc::new(Self::Layout {
            name,
            front_matter,
            content_node,
            parent,
        })
    }

    fn apply_all_substitutions(&self, s: String, context: TemplateContextPtr, global_context: &mut GlobalContext, front_matter: &FrontMatter) -> String {
        context.borrow_mut().add_front_matter(front_matter);
        let output = Self::perform_substitutions_strings(s, front_matter);
        Self::apply_substitutions(&output, context, global_context)
    }
    
    pub fn render(&self, context: TemplateContextPtr, global_context: &mut GlobalContext) -> String {
        match self {
            Self::Page { content_node, parent, front_matter, .. } => {
                let page_context = TemplateContext::new(Some(context.clone()));

                let output = self.apply_all_substitutions(
                    content_node.render(page_context.clone(), global_context),
                    page_context.clone(),
                    global_context,
                    front_matter
                );

                parent.as_ref().map_or(output.clone(), |parent| {
                    let layout_context = TemplateContext::new(Some(page_context));
                    layout_context.borrow_mut().strings.insert("content".to_string(), output);
                    parent.render(layout_context, global_context)
                })
            }
            Self::Layout { content_node, parent, front_matter, .. } => {
                let output = self.apply_all_substitutions(
                    content_node.render(context.clone(), global_context),
                    context.clone(),
                    global_context,
                    front_matter
                );
                
                parent.as_ref().map_or(output.clone(), |parent| {
                    let layout_context = TemplateContext::new(Some(context.clone()));
                    layout_context.borrow_mut().strings.insert("content".to_string(), output);
                    parent.render(layout_context, global_context)
                })
            }
            Self::IfBlock { condition, true_branch, false_branch } => {
                let ctx = context.borrow();
                if ctx.get_string(condition).is_some() {
                    true_branch.render(context.clone(), global_context)
                } else if let Some(false_branch) = false_branch {
                    false_branch.render(context.clone(), global_context)
                } else {
                    String::new()
                }
            }
            Self::ForEachBlock { key, item_name, body } => {
                let ctx = context.borrow();
                if let Some(Value::Sequence(items)) = ctx.json_data.get(key) {
                    items.iter()
                    .map(|item| {
                        let new_ctx = TemplateContext::new(Some(context.clone()));
                        if let Value::Mapping(map) = item {
                            for (k, v) in map {
                                if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                                    new_ctx.borrow_mut().strings.insert(k.to_string(), v.to_string());
                                }
                            }
                        }
                        body.render(new_ctx, global_context)
                    })
                    .collect()
                } else {
                    String::new()
                }
            }
            Self::Func { name, args, block_content } => {
                if let Some(func) = global_context.functions.get(name).cloned() {
                    func(args, block_content.as_deref(), context, global_context)
                } else {
                    name.clone()
                }
            }
            Self::StringContent(s) => s.clone(),
            Self::Composite(template_nodes) => {
                template_nodes.iter()
                .map(|x| x.render(context.clone(), global_context))
                .collect::<Vec<String>>()
                .join("")
            },
        }
    }
    
    fn perform_substitutions_str(s: String, k: &str, v: &str) -> String {
        let mut s = s;
        for k in &[format!(" {} ", k), k.to_string()] {
            for k in &[format!("{{{{{}}}}}", k), format!("{{{}}}", k)] {
                s = s.replace(k, v);
            }
        }
        s
    }
    
    fn perform_substitutions_strings(s: String, strings: &HashMap<String, String>) -> String {
        strings.iter().fold(s, |acc, (key, value)| {
            Self::perform_substitutions_str(acc, key, value)
        })
    }
    
    fn apply_substitutions(s: &str, context: TemplateContextPtr, global_context: &mut GlobalContext) -> String {
        let ctx = context.borrow();
        let mut output = Self::perform_substitutions_strings(s.to_string(), &ctx.strings);
        
        let rendered = ctx.nodes.iter()
            .map(|(k, v)| (k.clone(), v.render(context.clone(), global_context)))
            .collect::<HashMap<_, _>>();
        output = Self::perform_substitutions_strings(output, &rendered);
        
        if let Some(parent) = ctx.parent.clone() {
            Self::apply_substitutions(&output, parent, global_context)
        } else {
            global_context.site_strings.iter()
                .fold(output, |acc, (key, value)| Self::perform_substitutions_str(acc, key, value))
        }
    }
    
    pub fn print_tree(&self, indent: usize) {
        if let Some(parent) = self.get_parent() {
            parent.print_tree(indent);
            return;
        }

        match self {
            Self::Page { path, content_node, .. } => {
                println!("{:indent$}📄 {} (Page)", "", path, indent = indent);
                content_node.print_tree(indent + 1);
            }
            Self::Layout { name, content_node, .. } => {
                println!("{:indent$}📦 {} (Layout)", "", name, indent = indent);
                content_node.print_tree(indent + 1);
            }
            Self::IfBlock { condition, true_branch, false_branch } => {
                println!("{:indent$}❓ if {} (Conditional)", "", condition, indent = indent);
                println!("{:indent$}├── Then:", "", indent = indent + 2);
                true_branch.print_tree(indent + 4);
                if let Some(false_branch) = false_branch {
                    println!("{:indent$}└── Else:", "", indent = indent + 2);
                    false_branch.print_tree(indent + 4);
                }
            }
            Self::ForEachBlock { key, item_name, body } => {
                println!("{:indent$}🔄 foreach {} as {} (Loop)", "", key, item_name, indent = indent);
                body.print_tree(indent + 2);
            }
            Self::Func { name, args, block_content } => {
                println!("{:indent$}ƒ {} (Function)", "", name, indent = indent);
                println!("{:indent$}├── Args: {:?}", "", args, indent = indent + 2);
                if let Some(content) = block_content {
                    println!("{:indent$}└── Block: {}...", "", content.replace("\n", "").chars().take(30).collect::<String>(), indent = indent + 2);
                }
            }
            Self::StringContent(s) => {
                println!("{:indent$}📝 {}...", "", s.replace("\n", "").chars().take(50).collect::<String>(), indent = indent);
            }
            Self::Composite(nodes) => {
                if nodes.len() == 1 {
                    nodes.first().unwrap().print_tree(indent)
                } else {
                    println!("{:indent$}🧩 Composite ({} items)", "", nodes.len(), indent = indent);
                    nodes.iter().for_each(|node| node.print_tree(indent + 2));
                }
            }
        }
    }

    fn get_parent(&self) -> Option<&Rc<TemplateNode>> {
        match self {
            Self::Page { parent, .. } | Self::Layout { parent, .. } => parent.as_ref(),
            _ => None,
        }
    }
}

impl GlobalContext {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            layout_cache: HashMap::new(),
            site_strings: HashMap::new(),
            functions: HashMap::new(),
        }
    }

    pub fn new_with_defaults(cfg: Config) -> Self {
        let mut x = Self::new(cfg);
        x.with_default_strings();
        x.with_default_funcs();
        x.load_site_data();
        x
    }

    pub fn with_default_strings(&mut self) -> &mut Self {
        self.site_strings.insert("build_revision".to_string(), Self::get_git_revision());
        self
    }

    pub fn with_default_funcs(&mut self) -> &mut Self {
        self.register_function(
            "uppercase",
            &|args, _, _, _| args.first().map_or(String::new(), |s| s.to_uppercase()),
        );
        
        self.register_function(
            "lowercase",
            &|args, _, _, _| args.first().map_or(String::new(), |s| s.to_lowercase()),
        );

        self.register_function(
            "date",
            &|_, _, _, _| Local::now().format("%Y-%m-%d").to_string(),
        );

        self.register_function(
            "datetime",
            &|_, _, _, _| Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        );

        self.register_function(
            "datetime-pretty",
            &|_, _, _, _| Local::now().format("%c").to_string(),
        );

        self.register_function(
            "modified-datetime-pretty",
            &|_, _, ctx, _| {
                ctx.borrow().path // get mod time from file path
            }.format("%c").to_string(),
        );

        self.register_function(
            "date_html",
            &|_, _, ctx, _| {
                if let Some(date_str) = ctx.borrow().get_string("date") {
                    format!("<p><b>Date:</b> {}</p>", date_str)
                } else {
                    "".to_string()
                }
            },
        );

        self.register_function(
            "tags_html",
            &|_, _, ctx, _| {
                if let Some(tags_str) = ctx.borrow().get_string("tags") {
                    format!("<p><b>Tags:</b> {}</p>", tags_str)
                } else {
                    "".to_string()
                }
            },
        );

        self.register_function(
            "categories_html",
            &|_, _, ctx, _| {
                if let Some(categories_str) = ctx.borrow().get_string("categories") {
                    format!("<p><b>Categories:</b> {}</p>", categories_str)
                } else {
                    "".to_string()
                }
            },
        );

        self.register_function(
            "relative-url",
            &|args, _, _, ctx| {
                ctx.relative_url(args.get(0).unwrap())
            },
        );

        self.register_function("image_html", &|_, _, ctx, global| {
            if let Some(url) = ctx.borrow().get_string("image") {
                let url = global.relative_url(&url);
                format!("<img src=\"{}\" />", url)
            } else {
                "".to_string()
            }
        });

        self.register_function(
            "list_md",
            &|args, block, ctx, global| {
                let path = args.get(0).expect("list_md requires a path argument");
                let path = global.cfg.relative_to_config_path(&PathBuf::from(path));
                let template_name = args.get(1); // Optional template name

                // println!("called list_md with {} and {:?}", path, template_name);
                
                let mut items = vec![];
                
                // Read directory and process markdown files
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        items.push(
                            match global.build_page(entry.path().to_str().unwrap()) {
                                Ok(f) => f,
                                Err(e) => Rc::new(TemplateNode::StringContent(format!("error: {:?}", e))),
                            }
                        );
                    }
                }
                
                items.iter()
                    .map(|x| x.render(ctx.clone(), global))
                    .collect::<Vec<String>>()
                    .join("")
            },
        );

        self.register_function(
            "json_list",
            &|args, block, ctx, _| {
                let items_key = "items".to_string();
                let key = args.first().unwrap_or(&items_key);
                ctx.borrow().json_data.get(key)
                    .and_then(Value::as_sequence)
                    .map(|items| {
                        block.map_or_else(|| {
                            items.iter().filter_map(Value::as_mapping).fold(
                                String::from("<ul>\n"),
                                |mut output, obj| {
                                    output.push_str("<li>");
                                    if let Some(Value::String(title)) = obj.get("title") {
                                        output.push_str(&format!("<h3>{}</h3>", title));
                                    }
                                    if let Some(Value::String(desc)) = obj.get("description") {
                                        output.push_str(&format!("<p>{}</p>", desc));
                                    }
                                    output.push_str("</li>\n");
                                    output
                                },
                            ) + "</ul>"
                        }, |b| b.to_string())
                    })
                    .unwrap_or_default()
            },
        );
        self
    }

    fn register_function(&mut self, name: &str, func: &'static TemplateFunc) {
        self.functions.insert(name.to_string(), Rc::new(func));
    }
    
    pub fn get_layout(&mut self, name: &str) -> Rc<TemplateNode> {
        // println!("get_layout {}", name);
        if let Some(layout) = self.layout_cache.get(name) {
            return layout.clone();
        }
        
        let path = PathBuf::from("templates").join(format!("{}.tpl.html", name));
        let path = self.cfg.relative_to_config_path(&path);
        let content = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("Failed to read template: {} at {}", name, path.to_str().unwrap()));
        
        let (front_matter, html) = parse_front_matter(&content);
        let mut front_matter = parse_yaml_front_matter(front_matter).unwrap_or_default();
        if !front_matter.contains_key("layout") && name != "default" && name != "site" {
            front_matter.insert("layout".to_string(), "default".to_string());
        }

        Self::get_front_matter_json_data(&mut front_matter);

        // Check if this layout has a parent layout
        let parent_layout = if let Some(layout_name) = front_matter.get("layout") {
            if layout_name.is_empty() {
                None
            } else {
                Some(self.get_layout(&layout_name))
            }
        } else {
            None
        };
    
        // Parse control blocks in the content
        let content_node = self.parse_control_blocks(&html);
        
        let layout = TemplateNode::new_layout(name.to_string(), front_matter, content_node, parent_layout);
        self.layout_cache.insert(name.to_string(), layout.clone());
        layout
    }
    
    pub fn load_site_data(&mut self) {
        let path = self.cfg.relative_to_config_path(&PathBuf::from("data/site.yaml"));
        let path = path.to_str().unwrap();
        let site_yaml = self.load_yaml_data_merge_env_variant(path).expect(&format!("could not get {}", path));
        if let Value::Mapping(mapping) = site_yaml {
            self.load_site_data_from_yaml_mapping(mapping)
        } else {
            panic!("unsupported site yaml type")
        }
    }

    pub fn load_yaml_data_merge_env_variant(&self, path: &str)  -> Result<Value, Box<dyn Error>> {
        let primary = load_yaml_data(path)?;
        if let Some(path_env_secondary) = self.path_add_variant(path) {
            // Only merge if variant file exists
            if Path::new(&path_env_secondary).exists() {
                let secondary = load_yaml_data(&path_env_secondary)?;
                Ok(merge_yaml_values(primary, secondary))
            } else {
                Ok(primary)
            }
        } else {
            Ok(primary)
        }
    }

    pub fn path_add_variant(&self, path: &str) -> Option<String> {
        // change file path from something like "abcde.txt" to "abcde.blue.txt", "foobar.tpl.html" to "foobar.blue.tpl.html"
        if let Some(variant) = &self.cfg.variant {
            if !variant.trim().is_empty() {
                let path = Path::new(path);
                
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let ext = path.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| format!(".{}", e))
                        .unwrap_or_default();
                        
                    let new_filename = format!("{}.{}{}", stem, variant, ext);
                    return path.with_file_name(new_filename)
                        .to_str()
                        .map(|x| x.to_string());
                }
            }
        }
        None
    }
    
    pub fn load_site_data_from_yaml_mapping(&mut self, mapping: serde_yaml::Mapping) {
        for (k, v) in mapping.iter() {
            let v = if v.is_bool() {
                v.as_bool().unwrap().to_string()
            } else {
                v.as_str().unwrap().to_string()
            };
            self.site_strings.insert(k.as_str().unwrap().to_string(), v);
        }
    }
    
    fn get_git_revision() -> String {
        fn try_git_command(args: &[&str]) -> Option<String> {
            Command::new("git")
                .args(args)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
    
        // Try short hash first, then full hash
        try_git_command(&["rev-parse", "--short", "HEAD"])
            .or_else(|| try_git_command(&["rev-parse", "HEAD"]))
            .unwrap_or_else(|| {
                let timestamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs().to_string())
                    .unwrap_or_else(|_| "unknown_time".to_string());
                format!("nogit-{}", timestamp)
            })
    }
    

    fn parse_control_blocks(&self, content: &str) -> Rc<TemplateNode> {
        let mut nodes = Vec::new();
        let mut remaining = content;
        
        while let Some(open_pos) = remaining.find("{{") {
            let before = &remaining[..open_pos];
            if !before.is_empty() {
                nodes.push(TemplateNode::StringContent(before.to_string()));
            }
            
            let close_pos = remaining[open_pos..].find("}}").unwrap() + open_pos;
            let complete_tag = &remaining[open_pos..close_pos+2];
            let tag = &remaining[open_pos+2..close_pos].trim();
            remaining = &remaining[close_pos+2..];
            
            match tag.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["if", condition] => {
                    let (inner_content, new_remaining) = Self::parse_block_content(remaining, "endif");
                    remaining = new_remaining;
                    
                    // Split into if and else parts if needed
                    let (true_content, false_content) = match inner_content.split_once("{{ else }}") {
                        Some((true_part, false_part)) => (true_part, Some(false_part)),
                        None => (inner_content, None),
                    };
                    
                    let true_node = self.parse_control_blocks(true_content);
                    let false_node = false_content.map(|c| self.parse_control_blocks(c));
                    
                    nodes.push(TemplateNode::IfBlock {
                        condition: condition.to_string(),
                        true_branch: true_node,
                        false_branch: false_node,
                    });
                },
                ["else"] => {
                    // Only warn if this isn't part of a string that looks like a real else
                    if !tag.starts_with("else ") && !tag.ends_with(" else") {
                        eprintln!("Warning: found else without matching if in content: {:?}", tag);
                    }
                    // Skip this token and continue parsing
                },
                ["foreach", key, "as", item_name] => {
                    let (inner_content, new_remaining) = Self::parse_block_content(remaining, "endforeach");
                    remaining = new_remaining;
                    let inner_node = self.parse_control_blocks(inner_content);
                    nodes.push(TemplateNode::ForEachBlock {
                        key: key.to_string(),
                        item_name: item_name.to_string(),
                        body: inner_node,
                    });
                },
                _ => {
                    match Self::parse_function_call(tag) {
                        Some((name, args)) if self.functions.contains_key(name) => {
                            nodes.push(TemplateNode::Func {
                                name: name.to_string(),
                                args: args.iter().map(|s| s.to_string()).collect(),
                                block_content: None,
                            });
                        }
                        _ => {
                            nodes.push(TemplateNode::StringContent(complete_tag.to_string()));
                        }
                    }
                }
            }
        }
        
        if !remaining.is_empty() {
            nodes.push(TemplateNode::StringContent(remaining.to_string()));
        }
        
        Rc::new(TemplateNode::Composite(nodes))
    }

    fn parse_block_content<'a>(content: &'a str, end_tag: &str) -> (&'a str, &'a str) {
        let end_pattern = format!("{{{{ {end_tag} }}}}");
        let end_pos = content.find(&end_pattern).unwrap_or(content.len());
        (&content[..end_pos], &content[end_pos + end_pattern.len()..])
    }

    fn parse_function_call(tag: &str) -> Option<(&str, Vec<&str>)> {
        let mut parts = tag.split_whitespace();
        parts.next().map(|name| (name, parts.collect()))
    }

    fn build_page(
        &mut self,
        path: &str,
    ) -> Result<Rc<TemplateNode>, Box<dyn Error>> {
        let content = fs::read_to_string(path)?;
        let (front_matter, markdown) = parse_front_matter(&content);
        let mut front_matter = parse_yaml_front_matter(front_matter)?;
        
        // Set defaults
        // println!("page {} front_matter.keys: {}", path, front_matter.keys().into_iter().cloned().collect::<Vec<String>>().join(", "));
        if !front_matter.contains_key("layout") {
            front_matter.insert("layout".to_string(), "default".to_string());
        }
        if !front_matter.contains_key("title") {
            front_matter.insert("title".to_string(), 
            Path::new(path).file_stem().unwrap().to_string_lossy().into_owned());
        }

        Self::get_front_matter_json_data(&mut front_matter);
        
        // Convert markdown to HTML
        let mut html_content = String::new();
        let parser = pulldown_cmark::Parser::new_ext(markdown, Options::all())
            .map(|event| match event {
                // Rewrite links
                Event::Start(Tag::Link { dest_url, link_type, title, id }) => {
                    // println!("found link {}", dest_url);
                    let new_dest = self.relative_url(dest_url.as_ref());
                    Event::Start(Tag::Link { link_type, dest_url: new_dest.into(), title, id })
                }
                // Rewrite images
                Event::Start(Tag::Image { dest_url, link_type, title, id }) => {
                    // println!("found img {}", dest_url);
                    let new_dest = self.relative_url(dest_url.as_ref());
                    Event::Start(Tag::Image { link_type, dest_url: new_dest.into(), title, id })
                }
                // Pass through other events unchanged
                _ => event,
            });
        html::push_html(&mut html_content, parser);
        
        // Parse control blocks in the content
        let content_node = self.parse_control_blocks(&html_content);
        
        // Create output path
        let output_path = self.cfg.full_output_path()
            .join(file_path_stem(&self.cfg.full_input_path(), path))
            .with_extension("html");

        // println!("output_path: {:?}", output_path);
        
        // Get the layout hierarchy
        let layout = if let Some(layout_name) = front_matter.get("layout") {
            if layout_name.is_empty() {
                None
            } else {
                Some(self.get_layout(layout_name))
            }
        } else {
            None
        };
        
        // Create the page with the layout as parent
        Ok(TemplateNode::new_page(
            path.to_string(),
            front_matter,
            content_node,
            output_path,
            layout,
        ))
    }

    fn get_front_matter_json_data(front_matter: &mut HashMap<String, String>) {
        if let Some(json_path) = &front_matter.get("json_data") {
            let json_path = std::env::current_dir().unwrap().join(json_path).to_string_lossy().to_string();
            if let Ok(json_data) = load_yaml_data(json_path.as_str()) {
                // todo: implement returning the json/yaml and put it into current context
                // front_matter.insert("items".to_string(), format!("{:?}", json_data));
                // front_matter.insert_node("json_list".to_string(), TemplateNode::Json("items"));
            }
        }
    }

    fn relative_url(&self, path: &str) -> String {
        if has_protocol(path) || path.starts_with('#') {
            return path.to_string();
        }
        
        let mut base = self.site_strings.get("site.url").unwrap().to_owned();
        base = base.trim_end_matches('/').to_string();
        let path = path.trim_start_matches('/').to_string();
        format!("{}/{}", base, path)
    }

    fn load_robots_config(&self) -> Result<Option<RobotsConfig>, Box<dyn std::error::Error>> {
        let config_path = self.cfg.relative_to_config_path(&PathBuf::from("data/robots_config.yaml"));
        if fs::exists(&config_path)? {
            let mut file = File::open(config_path)?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            
            let config: RobotsConfig = serde_yaml::from_str(&contents)?;
            Ok(Some(config))
        } else {
            Ok(None)
        }
    }
}

impl SitemapXmlNode {
    // Creates a new sitemap entry with required URL
    pub fn new(loc: String) -> Self {
        Self {
            loc,
            lastmod: None,
            changefreq: None,
            priority: None,
            alternates: Vec::new(),
        }
    }
    
    // Sets the last modification date
    pub fn with_lastmod(mut self, lastmod: DateTime<Utc>) -> Self {
        self.lastmod = Some(lastmod);
        self
    }
    
    // Sets the change frequency
    pub fn with_changefreq(mut self, changefreq: ChangeFrequency) -> Self {
        self.changefreq = Some(changefreq);
        self
    }
    
    // Sets the priority (clamped between 0.0 and 1.0)
    pub fn with_priority(mut self, priority: f32) -> Self {
        self.priority = Some(priority.clamp(0.0, 1.0));
        self
    }
    
    // Adds an alternate language version
    pub fn add_alternate(mut self, url: String, lang: impl Into<String>) -> Self {
        self.alternates.push(AlternateLink {
            url,
            lang: lang.into(),
        });
        self
    }
    
    // Generates the XML for this sitemap entry
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        
        xml.push_str("<url>\n");
        xml.push_str(&format!("  <loc>{}</loc>\n", self.loc));
        
        if let Some(lastmod) = self.lastmod {
            xml.push_str(&format!("  <lastmod>{}</lastmod>\n", lastmod.to_rfc3339()));
        }
        
        if let Some(changefreq) = &self.changefreq {
            xml.push_str(&format!("  <changefreq>{}</changefreq>\n", changefreq));
        }
        
        if let Some(priority) = self.priority {
            xml.push_str(&format!("  <priority>{:.1}</priority>\n", priority));
        }
        
        if !self.alternates.is_empty() {
            for alt in &self.alternates {
                xml.push_str(&format!(
                    "  <xhtml:link rel=\"alternate\" hreflang=\"{}\" href=\"{}\"/>\n",
                    alt.lang, alt.url
                ));
            }
        }
        
        xml.push_str("</url>");
        xml
    }
    
    // Creates a node from a file path (relative to site root)
    pub fn from_file(
        file_path: PathBuf,
        site_url: &String,
        lastmod: Option<DateTime<Utc>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let relative_path = file_path.to_string_lossy().replace('\\', "/");
        let full_url = PathBuf::from(&site_url).join(&relative_path);
        
        Ok(Self::new(full_url.to_string_lossy().to_string())
            .with_lastmod(lastmod.unwrap_or_else(Utc::now)))
    }

    pub fn generate_sitemap_xml(nodes: &[SitemapXmlNode]) -> String {
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
        s.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">");

        for node in nodes {
            s.push_str(&node.to_xml());
        }

        s.push_str("</urlset>");
        s
    }
}

// ========== Helper Functions ==========

fn parse_front_matter(content: &str) -> (&str, &str) {
    content.strip_prefix("---")
        .and_then(|s| s.split_once("---"))
        .map(|(fm, rest)| (fm.trim(), rest.trim()))
        .unwrap_or(("", content))
}

fn parse_yaml_front_matter(front_matter: &str) -> Result<FrontMatter, Box<dyn Error>> {
    if front_matter.is_empty() {
        // println!("front_matter is empty");
        Ok(FrontMatter::new())
    } else {
        serde_yaml::from_str(front_matter)
            .map_err(|e| e.into())
    }
}
fn get_md_files_recursive(path: &Path) -> Vec<String> {
    // List of directories to ignore
    const IGNORED_DIRS: &[&str] = &["assets", "templates", "data"];
    
    fs::read_dir(path).ok()
        .map(|entries| {
            entries.filter_map(|entry| entry.ok())
                .flat_map(|entry| {
                    let path = entry.path();
                    
                    // Skip if filename starts with '_'
                    if path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.starts_with('_'))
                        .unwrap_or(false) 
                    {
                        return Vec::new();
                    }
                    
                    // Skip if it's an ignored directory
                    if path.is_dir() && path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|name| IGNORED_DIRS.contains(&name))
                        .unwrap_or(false)
                    {
                        return Vec::new();
                    }
                    
                    // Process directory or markdown file
                    if path.is_dir() {
                        get_md_files_recursive(&path)
                    } else if path.extension().map_or(false, |ext| ext == "md") {
                        path.to_str().map(|s| s.to_string()).into_iter().collect()
                    } else {
                        Vec::new()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn file_path_stem(base_path: &Path, full_path: &str) -> String {
    Path::new(full_path).strip_prefix(base_path)
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or_else(|_| full_path.to_string())
}

fn has_protocol(url: &str) -> bool {
    // Split at first colon to check for protocol
    if let Some(colon_pos) = url.find(':') {
        // Ensure there's at least one character before colon
        if colon_pos > 0 {
            // Check if the part before colon is all alphabetic (protocol name)
            let protocol = &url[..colon_pos];
            protocol.chars().all(|c| c.is_ascii_alphabetic())
        } else {
            false
        }
    } else {
        false
    }
}

fn copy_assets(src: &str, dst: &str, verbose: bool) -> Result<(), Box<dyn Error>> {
    if !Path::new(src).exists() {
        println!("input assets dir {} does not exist", src);
        return Ok(());
    } else if src == dst {
        println!("copy_assets src ({}) == dst ({}), do nothing", src, dst);
        return Ok(());
    }
    
    create_dir(&Path::new(dst), verbose)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = Path::new(dst).join(entry.file_name());
        
        if path.is_dir() {
            copy_assets(path.to_str().unwrap(), dest_path.to_str().unwrap(), verbose)?;
        } else {
            fs::copy(path, dest_path)?;
        }
    }
    Ok(())
}

fn load_yaml_data(path: &str) -> Result<Value, Box<dyn Error>> {
    let file = File::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path, e))?;
    serde_yaml::from_reader(file)
        .map_err(|e| format!("Failed to parse YAML in {}: {}", path, e).into())
}

// Helper function to deep merge two YAML values
fn merge_yaml_values(mut primary: Value, secondary: Value) -> Value {
    if let Value::Mapping(ref mut map1) = primary {
        if let Value::Mapping(map2) = secondary {
            for (k, v) in map2 {
                map1.insert(k, v);
            }
        }
    }
    primary
}

pub fn generate_robots_txt(
    config: &RobotsConfig,
    html_files: &[PathBuf],
    output_dir: &PathBuf,
) -> String {
    let mut robots = String::new();
    
    // Add sitemap if specified
    if let Some(sitemap) = &config.sitemap {
        robots.push_str(&format!("Sitemap: {}\n\n", sitemap));
    }
    
    // Add global rules
    if let Some(global_rules) = &config.global_rules {
        // Global crawl delay
        if let Some(delay) = config.crawl_delay {
            robots.push_str(&format!("Crawl-delay: {}\n", delay));
        }
        
        // Global allow rules
        if let Some(allow) = &global_rules.allow {
            for path in allow {
                robots.push_str(&format!("Allow: {}\n", path));
            }
        }
        
        // Global disallow rules
        if let Some(disallow) = &global_rules.disallow {
            for path in disallow {
                robots.push_str(&format!("Disallow: {}\n", path));
            }
        }
        
        robots.push('\n');
    }
    
    // Add user-agent specific rules
    if let Some(user_agents) = &config.user_agents {
        for agent_rule in user_agents {
            // Write user-agent line(s)
            for agent in &agent_rule.user_agents {
                robots.push_str(&format!("User-agent: {}\n", agent));
            }
            
            // Agent-specific crawl delay
            if let Some(delay) = agent_rule.crawl_delay {
                robots.push_str(&format!("Crawl-delay: {}\n", delay));
            }
            
            // Agent-specific allow rules
            if let Some(allow) = &agent_rule.allow {
                for path in allow {
                    robots.push_str(&format!("Allow: {}\n", path));
                }
            }

            if config.auto_include_generated_html.unwrap_or(false) {
                // Auto-include any generated HTML files
                robots.push_str("# Auto-included generated files\n");
                for path in html_files {
                    robots.push_str(&format!("Allow: {}\n", path.to_str().unwrap()));
                }
                robots.push('\n');
            }
            
            // Agent-specific disallow rules
            if let Some(disallow) = &agent_rule.disallow {
                for path in disallow {
                    robots.push_str(&format!("Disallow: {}\n", path));
                }
            }
            
            robots.push('\n');
        }
    }
    
    if config.auto_disallow_non_included_html.unwrap_or(false) {
        // Auto-disallow any HTML files not marked for inclusion
        let allowed_paths = get_all_allowed_paths(config);
        let disallowed_html = find_disallowed_html(html_files, &allowed_paths, &output_dir);
        
        if !disallowed_html.is_empty() {
            robots.push_str("# Auto-disallowed generated files\n");
            for agent in get_all_user_agents(config) {
                robots.push_str(&format!("User-agent: {}\n", agent));
                for path in &disallowed_html {
                    robots.push_str(&format!("Disallow: {}\n", path));
                }
                robots.push('\n');
            }
        }
    }
    
    robots
}

// Helper function to get all allowed paths from config
fn get_all_allowed_paths(config: &RobotsConfig) -> Vec<String> {
    let mut allowed = Vec::new();
    
    if let Some(global_rules) = &config.global_rules {
        if let Some(paths) = &global_rules.allow {
            allowed.extend(paths.iter().cloned());
        }
    }
    
    if let Some(user_agents) = &config.user_agents {
        for agent in user_agents {
            if let Some(paths) = &agent.allow {
                allowed.extend(paths.iter().cloned());
            }
        }
    }
    
    allowed
}

// Helper function to get all user agents from config
fn get_all_user_agents(config: &RobotsConfig) -> Vec<String> {
    let mut agents = vec![];
    
    if let Some(user_agents) = &config.user_agents {
        for agent_rule in user_agents {
            agents.extend(agent_rule.user_agents.iter().cloned());
        }
    }
    
    agents.sort();
    agents.dedup();
    agents
}

// Find HTML files that shouldn't be indexed
fn find_disallowed_html(
    html_files: &[PathBuf],
    allowed_paths: &[String],
    output_dir: &PathBuf,
) -> Vec<String> {
    html_files
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(output_dir).ok()?;
            let web_path = format!("/{}", relative.display().to_string().replace('\\', "/"));
            
            // Check if this path is explicitly allowed
            if !allowed_paths.iter().any(|allowed| {
                // Simple prefix matching - you might want more sophisticated matching
                web_path.starts_with(allowed)
            }) {
                Some(web_path)
            } else {
                None
            }
        })
        .collect()
}

fn generate_and_write_sitemap_xml(verbose: bool, output_base: &PathBuf, sitemap_xml_nodes: Vec<SitemapXmlNode>) -> Result<(), Box<dyn Error>> {
    if verbose {
        println!("generating sitemap.xml");
    }
    let sitemap_xml = SitemapXmlNode::generate_sitemap_xml(&sitemap_xml_nodes);
    let output_path = output_base.join("assets/sitemap.xml");
    fs::write(output_path, sitemap_xml)?;
    Ok(())
}

fn generate_and_write_robots_txt(verbose: bool, output_base: &PathBuf, output_html_paths: Vec<PathBuf>, robots_config: RobotsConfig) -> Result<(), Box<dyn Error>> {
    if verbose {
        println!("Generating robots.txt");
    }
    let content = generate_robots_txt(&robots_config, &output_html_paths, output_base);
    let output_path = output_base.join("assets/robots.txt");
    fs::write(output_path, content)?;
    Ok(())
}

// ========== Main Function ==========

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    
    // Load config file if specified
    let config = if let Some(config_path) = &cli.config {
        Config::from_file(config_path)?
    } else {
        // Try default config locations
        let default_config_path = std::env::current_dir().unwrap().join("meowdown-config.yaml");
        if std::fs::exists(&default_config_path)? {
            Config::from_file(&default_config_path)?
        } else {
            // or create empty
            Config::default()
        }
    };
    
    if cli.verbose {
        println!("Starting with config: {:#?}", config);
    }

    match &cli.command {
        Some(Commands::Build { clean }) => {
            if *clean {
                clean_output_dir(&config)?;
            }
            build_site_for_each_variant(&config, cli.verbose)?;
        }
        Some(Commands::Clean { }) => {
            clean_output_dir(&config)?;
        }
        Some(Commands::Watch { }) => {
            watch_and_rebuild(&config, cli.verbose)?;
        }
        Some(Commands::New { name, default }) => {
            create_new_project(name, *default, cli.verbose)?;
        }
        None => {
            // Default to build command
            build_site_for_each_variant(&config, cli.verbose)?;
        }
    }
    Ok(())
}

pub fn watch_and_rebuild(
    config: &Config,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🔭 {} {}", 
        "Watching for changes...",
        "(Press Ctrl+C to stop)"
    );

    // Create channel for file change events
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher: RecommendedWatcher = Watcher::new(
        tx, 
        notify::Config::default()
            .with_poll_interval(Duration::from_secs(1)) // Debounce time
    )?;

    // Watch relevant directories
    let watch_dirs = [
        config.full_input_path(),
        PathBuf::from("assets"),
    ];

    for dir in watch_dirs {
        if dir.exists() {
            watcher.watch(&dir, notify::RecursiveMode::Recursive)?;
            if verbose {
                println!("👀 {}", format!("Watching: {}", dir.display()));
            }
        }
    }

    // Track last build time to avoid rapid rebuilds
    let mut last_build = std::time::Instant::now();
    let min_rebuild_interval = Duration::from_secs(2);

    loop {
        match rx.recv() {
            Ok(Ok(notify::Event { kind: notify::EventKind::Modify(_), paths, .. })) => {
                // Filter relevant changes
                if should_trigger_rebuild(&paths) && last_build.elapsed() > min_rebuild_interval {
                    if verbose {
                        println!("\n📡 {} {:?}", 
                            "Change detected in:",
                            paths.iter().map(|p| p.display()).collect::<Vec<_>>()
                        );
                    }

                    match build_site_for_each_variant(config, verbose) {
                        Ok(_) => {
                            println!("✅ {}", "Rebuild successful!");
                            last_build = std::time::Instant::now();
                        }
                        Err(e) => {
                            println!("❌ {}: {}", "Build failed", e);
                        }
                    }
                }
            }
            Ok(Err(e)) => println!("⚠️ {}: {}", "Watch error", e),
            _ => {}
        }
    }
}

fn should_trigger_rebuild(paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| {
        // Only trigger for these file types
        match p.extension().and_then(|e| e.to_str()) {
            Some("md" | "tpl" | "html" | "css" | "js" | "yml" | "yaml" | "json" | "csv") => true,
            _ => false
        }
    })
}

pub fn create_new_project(
    name: &str,
    use_default_template: bool,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let project_dir = std::env::current_dir().unwrap().join(name);
    println!("creating new project at {:?}", project_dir);
    
    // Create project directory structure
    create_dir(&project_dir, verbose)?;
    create_dir(&project_dir.join("assets"), verbose)?;
    create_dir(&project_dir.join("data"), verbose)?;
    create_dir(&project_dir.join("templates"), verbose)?;

    // Create default files
    create_file(
        &project_dir.join("data/site.yaml"),
        include_str!("../_default-data/data/default_site.yaml"),
        verbose,
    )?;

    create_file(
        &project_dir.join("meowdown-config.yaml"),
        include_str!("../_default-data/default_meowdown-config.yaml"),
        verbose,
    )?;

    create_file(
        &project_dir.join("index.md"),
        include_str!("../_default-data/default_index.md"),
        verbose,
    )?;

    if use_default_template {
        create_file(
            &project_dir.join("templates/default.tpl.html"),
            include_str!("../_default-data/templates/default_layout.tpl.html"),
            verbose,
        )?;
        
        create_file(
            &project_dir.join("assets/style.css"),
            include_str!("../_default-data/assets/default_style.css"),
            verbose,
        )?;
    }

    println!("✨ Created new project '{}' successfully!", name);

    if !use_default_template {
        println!("⚠️ No templates were included. Add your own in templates/");
    }

    Ok(())
}

// Helper function to create directory with verbose output
fn create_dir(path: &Path, verbose: bool) -> std::io::Result<()> {
    if verbose {
        println!("Creating directory: {}", path.display());
    }
    fs::create_dir_all(path)
}

// Helper function to create file with verbose output
fn create_file(path: &Path, content: &str, verbose: bool) -> std::io::Result<()> {
    if verbose {
        println!("Creating file: {}", path.display());
    }
    let mut file = File::create(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn clean_output_dir(config: &Config) -> Result<(), Box<dyn Error>> {
    let output = config.full_output_path();
    fs::remove_dir_all(output)
        .map_err(|e| e.into())
}

fn build_site_for_each_variant(config: &Config, verbose: bool) -> Result<(), Box<dyn Error>> {
    if config.variant.is_some() {
        if config.variants.is_some() {
            panic!("Cannot specify both variant and variants in {:?}", config.config_path);
        } else {
            build_site(config, verbose)
        }
    } else if let Some(variants) = &config.variants {
        for variant in variants {
            let cfg_variant = Config { variant: Some(variant.clone()), variants: None, .. config.clone() };
            build_site(&cfg_variant, verbose)?;
        }
        Ok(())
    } else {
        build_site(config, verbose)
    }
}

fn build_site(config: &Config, verbose: bool) -> Result<(), Box<dyn Error>> {
    let output_base = config.full_output_path();
    if verbose {
        println!("outputting to {}", output_base.to_str().unwrap());
    }

    let mut global_context = GlobalContext::new_with_defaults(config.clone());
    create_dir(&output_base, verbose)?;
    
    // Build and render all pages
    let mut output_html_paths = vec![];
    let mut sitemap_xml_nodes = vec![];
    for path in get_md_files_recursive(&config.full_input_path())
        .into_iter()
        .filter(|p| !p.contains("/assets/") && !p.contains("assets/"))
    {
        let page = global_context.build_page(&path)?;
        
        if verbose {
            // Print the tree structure
            page.print_tree(0);
        }
        
        if let TemplateNode::Page { path, output_path, front_matter, .. } = &*page {
            let ctx = TemplateContext::new(None);
            ctx.borrow_mut().add_front_matter(front_matter);
            
            create_dir(output_path.parent().unwrap(), verbose)?;

            if verbose {
                println!("writing html to {}", output_path.to_str().unwrap());
            }
            let relative_path = PathBuf::from(&output_path.to_str().unwrap()[output_base.to_str().unwrap().len()..]);
            output_html_paths.push(relative_path.clone());

            let lastmod = fs::File::open(path)
                .map(|f| f.metadata().map(|t| t.modified().ok()).ok()).ok()
                .flatten().flatten();
            sitemap_xml_nodes.push(SitemapXmlNode {
                changefreq: Some(ChangeFrequency::Monthly),
                loc: global_context.relative_url(relative_path.to_str().unwrap()),
                lastmod: lastmod.map(|x| x.into()),
                priority: None,
                alternates: vec![],
            });
            fs::write(output_path, page.render(ctx, &mut global_context))?;
        } else {
            panic!("could not build page {}", path);
        }
    }
    
    copy_assets(
        config.relative_to_config_path(&PathBuf::from("assets")).to_str().unwrap(), 
        output_base.join("assets").to_str().unwrap(), 
        verbose
    )?;

    match global_context.load_robots_config()? {
        Some(robots_config) if config.generate_robots_txt.unwrap_or(false) => {
            generate_and_write_sitemap_xml(verbose, &output_base, sitemap_xml_nodes)?;
            generate_and_write_robots_txt(verbose, &output_base, output_html_paths, robots_config)?;
        },
        _ if config.generate_sitemap_xml.unwrap_or(false) => {
            generate_and_write_sitemap_xml(verbose, &output_base, sitemap_xml_nodes)?;
        },
        _ => {
            if verbose {
                println!("Not generating sitemap.xml or robots.txt");
            }
        }
    }

    if let Some(variant) = &config.variant {
        println!("Site generation for variant {} complete!", variant);
    } else {
        println!("Site generation complete!");
    }
    Ok(())
}

#[derive(Parser)]
#[command(name = "MeowDown")]
#[command(version = "1.0")]
#[command(about = "A static site generator", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    
    // Path to config file (optional)
    #[arg(short, long)]
    config: Option<PathBuf>,
    
    // Verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(clap::Subcommand)]
enum Commands {
    // Build the site
    Build {    
        // Clean output directory before building
        #[arg(short, long)]
        clean: bool,
    },
    // Clean project
    Clean { },
    // Watch for changes and rebuild
    Watch { },
    // Create a new project
    New {
        // Project name
        name: String,
        
        // Use default template
        #[arg(short, long)]
        default: bool,
    },
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: None,
            input_dir: "./".to_string(),
            output_dir: "output".to_string(),
            watch: false,
            variant: None,
            variants: None,
            generate_robots_txt: None,
            generate_sitemap_xml: None,
        }
    }
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }
    
    fn relative_to_config_path(&self, path: &PathBuf) -> PathBuf {
        if let Some(p) = self.config_path.clone() {
            if path.as_os_str() == "." || path.as_os_str() == "./" {
                return PathBuf::from(p.as_str());
            } else {
                PathBuf::from(p.as_str()).parent().unwrap().into()
            }
        } else {
            std::env::current_dir().unwrap()
        }.join(path)
    }
    
    fn full_output_path(&self) -> PathBuf {
        if self.variants.is_some() {
            panic!("must call build_site_for_each_variant otherwise not sure which to build for");
        }

        let p = if let Some(variant) = &self.variant {
            self.output_dir.replace("{{variant}}", variant)
        } else {
            self.output_dir.clone()
        };

        if p.is_empty() || p == "." || p == "./" {
            self.config_path.clone().map(|x| PathBuf::from(x)).or(std::env::current_dir().ok()).unwrap()
        } else {
            self.relative_to_config_path(&PathBuf::from(&p))
        }
    }
    
    pub fn full_input_path(&self) -> PathBuf {
        let p = if let Some(variant) = &self.variant {
            self.input_dir.replace("{{variant}}", variant)
        } else {
            self.input_dir.clone()
        };

        if p.is_empty() || p == "." || p == "./" {
            self.config_path.clone().map(|x| PathBuf::from(x)).or(std::env::current_dir().ok()).unwrap()
        } else {
            self.relative_to_config_path(&PathBuf::from(&p))
        }
    }
}