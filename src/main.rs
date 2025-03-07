use anyhow::{bail, Context, Result};
use bytesize::ByteSize;
use indoc::formatdoc;
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    collections::{hash_map::DefaultHasher, BTreeMap, HashSet},
    fs::File,
    hash::{Hash, Hasher},
    io::{stdin, stdout, Cursor, Read, Write},
    ops::Range,
    path::Path,
    process::Command,
    rc::Rc,
    str::FromStr,
    time::Instant,
    vec,
};
use tempfile::TempDir;
use xz2::{read::XzEncoder, stream::LzmaOptions};

use crate::config::{Config, TemplateConfig};
use crate::synctex::Scanner;

mod config;
mod svg_optimize;
mod svg_utils;
mod synctex;

fn main() -> Result<()> {
    let mut buffer = String::new();
    let _ = stdin().read_to_string(&mut buffer)?;
    let mut tree = Value::from_str(&buffer)?;
    let config = Config::load(&tree)?;
    config.sanity_check()?;
    FragmentRenderer::new(config).render_with_latex(&mut tree)?;
    let output = serde_json::to_vec(&tree)?;
    stdout().write_all(&output)?;
    Ok(())
}

#[derive(Debug)]
struct FragmentRenderer<'a> {
    config: Config,
    fragments: Vec<Fragment<'a>>,
}

#[derive(Debug)]
struct Fragment<'a> {
    ty: FragmentType,
    src: String,
    refs: Vec<FragmentNodeRef<'a>>,
}

#[derive(Debug)]
enum FragmentNodeRef<'a> {
    Inline(&'a mut Value),
    Block(&'a mut Value),
}

#[derive(Debug)]
enum FragmentType {
    /// For ordinary inline maths.
    InlineMath(Style),
    /// For display maths.
    DisplayMath,
    /// These will be included in the .tex file without being surrounded by "{}".
    RawBlock,
    /// For display maths starting with %dontshow. They are included in the tex files but not shown.
    /// Use them for macro definitions.
    DontShow,
}

// On style: technically the correct way to handle styles is to handle find a set or orthogonal
// properties and make a product type out of it. But this is not extensible in a sense that
// orthogonality might be broken as new styles are considered. So instead we here just consider
// style to be an ordered list of style elements. The problem with this approach, however, is that
// it becomes difficult to compare equivalence of styles. Is Strong then Emph equivalent to Emph
// then Strong? Is nested Quote equivalent to single Quote? Equivalence of styles is necessary to
// deduplicate fragments and reduce size of our output. Of course for sane inputs this wouldn't be
// a problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleElement {
    Header(u64),
    Quote,
    Strong,
    Emph,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Inline math style.
enum Style {
    Plain,
    Fancy { base: Rc<Style>, this: StyleElement },
}

impl Style {
    fn push(self, new: StyleElement) -> Self {
        Self::Fancy {
            base: Rc::new(self),
            this: new,
        }
    }

    fn template(&self, config: &TemplateConfig) -> String {
        match self {
            Style::Plain => config.inline_math_inner.clone(),
            Style::Fancy { base, this } => {
                let base_template = base.template(config);
                let this_template = match this {
                    StyleElement::Header(level) => &config.header[*level as usize - 1],
                    StyleElement::Quote => &config.quote,
                    StyleElement::Strong => &config.strong,
                    StyleElement::Emph => &config.emph,
                };
                this_template.replace(&config.placeholder, &base_template)
            }
        }
    }
}

impl<'a> FragmentRenderer<'a> {
    fn new(config: Config) -> Self {
        Self {
            config,
            fragments: vec![],
        }
    }

    fn add_fragment(&mut self, ty: FragmentType, src: &str, node_ref: FragmentNodeRef<'a>) {
        match ty {
            // Inline fragments are often duplicates of previous ones encountered.
            // Caveat: if inline fragments contain expansions of macro with side effect (which is
            // rather unlikely), then this could cause trouble!
            FragmentType::InlineMath(ref styles) => {
                let src = src.trim();
                for item in self.fragments.iter_mut() {
                    match item.ty {
                        FragmentType::InlineMath(ref rstyles)
                            if item.src == src && styles == rstyles =>
                        {
                            item.refs.push(node_ref);
                            return;
                        }
                        _ => continue,
                    }
                }
                self.fragments.push(Fragment {
                    ty,
                    src: src.into(),
                    refs: vec![node_ref],
                });
            }
            _ => {
                self.fragments.push(Fragment {
                    ty,
                    src: src.trim().into(),
                    refs: vec![node_ref],
                });
            }
        }
    }

    fn generate_latex_with_line_mappings(&self) -> (String, Vec<Range<usize>>) {
        let mut lines: Vec<Range<usize>> = vec![];
        let mut output = String::new();
        let preamble_trimmed = self.config.preamble.trim_end();
        output.push_str(preamble_trimmed);
        output.push('\n');
        let mut current_line = preamble_trimmed.lines().count() + 1;
        for item in self.fragments.iter() {
            let template_config = &self.config.template;
            let expanded = match &item.ty {
                FragmentType::InlineMath(style) => {
                    let inner = style
                        .template(template_config)
                        .replace(&template_config.placeholder, &item.src);
                    self.config
                        .template
                        .inline_math
                        .replace(&template_config.placeholder, &inner)
                }
                FragmentType::DisplayMath => template_config
                    .display_math
                    .replace(&template_config.placeholder, &item.src),
                FragmentType::RawBlock | FragmentType::DontShow => item.src.clone(),
            };
            let expanded = expanded.trim_end();
            let start_line = current_line;
            output.push_str(expanded);
            current_line += expanded.lines().count();
            lines.push(start_line..current_line);
            output.push_str("\n\n");
            current_line += 1;
        }
        output.push_str(&self.config.postamble);
        (output, lines)
    }

    /// Scans and modifies the tree in-place, replacing all inline and display maths with rendered
    /// SVGs.
    pub fn render_with_latex(mut self, tree: &'a mut Value) -> Result<()> {
        let final_node = self.walk_and_create_final_node(tree)?;

        if self.fragments.is_empty() {
            // Make the final node a dummy.
            *final_node = json!({"t": "RawBlock", "c": ["html", ""]});
            return Ok(());
        }

        // In TeX 1 in = 72.72 pt = 72 bp, while in SVG 1 in = 72 pt.
        // Due to different definitions of pt we need a small scaling factor here.
        // See https://github.com/mgieseki/dvisvgm/issues/185
        const TEX2SVG_SCALING: f64 = 72.0 / 72.27;

        let (source_str, lines) = self.generate_latex_with_line_mappings();
        let working_dir = match self.config.output_folder {
            Some(_) => None,
            None => Some(TempDir::new()?),
        };
        let working_path = match &working_dir {
            Some(working_dir) => working_dir.path().to_path_buf(),
            None => Path::new(&self.config.output_folder.unwrap()).to_path_buf(),
        }
        .canonicalize()?;
        let source_path = working_path.join("source.tex");

        // eprintln!("{}", source_str);
        {
            let mut source = File::create(&source_path)?;
            source.write_all(source_str.as_bytes())?;
        }
        let mut latex_command = Command::new(self.config.latex);
        if self.config.mode == "dvi" {
            latex_command.arg("-output-format=dvi");
        } else if self.config.mode == "xdv" {
            latex_command.arg("--no-pdf");
        }
        let pdf_path = working_path.join(if self.config.mode == "pdf" {
            "source.pdf"
        } else if self.config.mode == "dvi" {
            "source.dvi"
        } else {
            "source.xdv"
        });
        let latex_command = latex_command
            .args([
                "-synctex=-1",
                "-interaction=nonstopmode",
                source_path.to_str().unwrap(),
            ])
            .current_dir(&working_path)
            .output()?;
        if !latex_command.status.success() {
            let error_message = String::from_utf8_lossy(&latex_command.stdout);
            eprintln!("latex error: {error_message}");
            bail!("fail to run latex: {error_message}",);
        }

        if self.config.mode == "dvi" {
            let _cst_command = Command::new("dvipdfm")
                .args([&pdf_path]).current_dir(&working_path).output()?;
        }

        let mut dvisvgm_command = Command::new(self.config.dvisvgm);
        if self.config.mode == "pdf" {
            dvisvgm_command.arg("--pdf");
        } else {
            dvisvgm_command.arg("--font-format=ttf");
        }
        let dvisvgm_command = dvisvgm_command
            .args([
                "--stdout",
                "--relative", // Empirically reduces SVG size.
                "--page=1-",  // Convert all pages.
                pdf_path.to_str().unwrap(),
            ])
            .current_dir(&working_path)
            .output()?;
        if !dvisvgm_command.status.success() {
            bail!(
                "fail to run dvisvgm: {}",
                String::from_utf8_lossy(&dvisvgm_command.stderr).trim()
            );
        }
        // Split svgs because we might have multiple pages.
        let svg_data = svg_utils::split_svgs(&dvisvgm_command.stdout)?;
        let svgs = svg_data
            .iter()
            .map(|svg_data| svg_utils::parse_to_tree(svg_data))
            .collect::<Result<Vec<_>, _>>()?;

        // A unique class name for each svg is important because HTMLs from multiple posts
        // may be put together in the home page of a blog. Then the decompressing code of each page
        // starts a race, each trying to modify every fragment image.
        let svg_class_names = svg_data
            .iter()
            .map(|svg| {
                let mut hasher = DefaultHasher::new();
                svg.hash(&mut hasher);
                let hash = hasher.finish();
                format!("jl-{}", base64::encode(hash.to_be_bytes()))
            })
            .collect::<Vec<_>>();

        let bboxes = svgs
            .iter()
            .map(svg_utils::paths_to_bboxes)
            .collect::<Vec<_>>();
        let scanner = Scanner::new(pdf_path, &working_path);
        let mut seen_boxes = HashSet::new();

        for (item, line_range) in self.fragments.iter_mut().zip(lines) {
            if let FragmentType::DontShow = item.ty {
                // Skip dont shows.
                for node in item.refs.iter_mut() {
                    match node {
                        FragmentNodeRef::Inline(node) => {
                            **node = json!({"t": "RawInline", "c": ["html", ""]})
                        }
                        FragmentNodeRef::Block(node) => {
                            **node = json!({"t": "RawBlock", "c": ["html", ""]});
                        }
                    }
                }
                continue;
            }

            #[derive(Clone, Debug)]
            struct Region {
                x_range: (f64, f64),
                y_range: (f64, f64),
                baseline: f64,
                baseline_width: f64,
            }

            let mut regions: BTreeMap<u32, Region> = BTreeMap::new();

            for line in line_range {
                for tb in scanner.query(line) {
                    let area = tb.width * (tb.height + tb.depth);
                    if area.into_inner() <= 1e-6 {
                        // Skip zero-area boxes. They may be generated by the TeX page breaker and
                        // do not actually correspond to anything in our source file. Also they
                        // wouldn't contribute to updating the region of the page anyways.
                        continue;
                    }
                    if seen_boxes.contains(&tb) {
                        // Continue if we have seen this box -- then probably that's SyncTeX's
                        // fault
                        continue;
                    }
                    seen_boxes.insert(tb.clone());

                    let (x_low, x_high) = (tb.h.into_inner(), (tb.h + tb.width).into_inner());
                    let (y_low, y_high) = (
                        (tb.v - tb.height).into_inner(),
                        (tb.v + tb.depth).into_inner(),
                    );
                    regions
                        .entry(tb.page)
                        .and_modify(|r| {
                            r.x_range = (r.x_range.0.min(x_low), r.x_range.1.max(x_high));
                            r.y_range = (r.y_range.0.min(y_low), r.y_range.1.max(y_high));
                            if tb.width.into_inner() > r.baseline_width {
                                r.baseline_width = tb.width.into_inner();
                                r.baseline = tb.v.into_inner();
                            }
                        })
                        .or_insert_with(|| Region {
                            x_range: (x_low, x_high),
                            y_range: (y_low, y_high),
                            baseline: tb.v.into(),
                            baseline_width: tb.width.into(),
                        });
                }
            }

            if regions.is_empty() {
                bail!("no boxes for {}", item.src);
            }
            if matches!(item.ty, FragmentType::InlineMath(_)) && regions.len() > 1 {
                bail!(
                    "inline fragments '{}' spans multiple pages {:?} (did you disable page numbering?)",
                    item.src,
                    regions.keys().collect::<Vec<_>>()
                );
            }

            let mut imgs = vec![];
            for (
                page,
                Region {
                    mut x_range,
                    mut y_range,
                    mut baseline,
                    ..
                },
            ) in regions.into_iter()
            {
                let svg_idx = page as usize - 1;
                // For whatever reason, the coordinate system of SVGs resulting from PDF
                // conversion is translated.
                let (x_base, y_base) = if self.config.mode == "pdf" {
                    let view_box = &svgs[svg_idx].svg_node().view_box.rect;
                    (view_box.left(), view_box.top())
                } else {
                    (0.0, 0.0)
                };
                // Convert everything from TeX coordinates to SVG coordinates.
                y_range = (
                    y_range.0 * TEX2SVG_SCALING + y_base,
                    y_range.1 * TEX2SVG_SCALING + y_base,
                );
                x_range = (
                    x_range.0 * TEX2SVG_SCALING + x_base,
                    x_range.1 * TEX2SVG_SCALING + x_base,
                );
                baseline = baseline * TEX2SVG_SCALING + y_base;

                if let FragmentType::DisplayMath | FragmentType::RawBlock = item.ty {
                    y_range = svg_utils::refine_y_range(
                        &bboxes[svg_idx],
                        y_range.0,
                        y_range.1,
                        self.config.y_range_tol,
                    );
                }
                y_range.0 -= self.config.y_range_margin;
                y_range.1 += self.config.y_range_margin;

                let depth = match item.ty {
                    FragmentType::InlineMath(_) => y_range.1 - baseline,
                    FragmentType::DisplayMath | FragmentType::RawBlock => 0.0,
                    FragmentType::DontShow => unreachable!(),
                };
                let extra_style = match item.ty {
                    FragmentType::InlineMath(_) => format!(
                        "top:{depth:.2}pt;margin-top:{neg_depth:.2}pt;position:relative;{extra_style}",
                        depth = depth - self.config.baseline_rise,
                        neg_depth = self.config.baseline_rise - depth,
                        extra_style = self.config.extra_style_inline
                    ),
                    FragmentType::DisplayMath | FragmentType::RawBlock => {
                        self.config.extra_style_display.clone()
                    }
                    FragmentType::DontShow => unreachable!(),
                };
                imgs.push(formatdoc!(
                    r##"<img src="#svgView(viewBox({x:.2},{y:.2},{width:.2},{height:.2}))"
                         class="{class_name} jl-{ty}" alt = "{alt}"
                         style="width:{width:.2}pt;height:{height:.2}pt;
                         display:inline;{extra_style}">"##,
                    x = x_range.0,
                    y = y_range.0,
                    width = x_range.1 - x_range.0,
                    height = y_range.1 - y_range.0,
                    ty = if let FragmentType::InlineMath(_) = item.ty {
                        "inline"
                    } else {
                        "display"
                    },
                    class_name = svg_class_names[svg_idx],
                    alt = html_escape::encode_text(&item.src),
                    extra_style = extra_style
                ));
            }
            let html = match item.ty {
                FragmentType::InlineMath(_) => imgs.join(""),
                FragmentType::DisplayMath | FragmentType::RawBlock => {
                    format!(
                        r#"<div class="jl-display-div" style="text-align:center;">{}</div>"#,
                        imgs.join("<br>")
                    )
                }
                FragmentType::DontShow => unreachable!(),
            };
            for node in item.refs.iter_mut() {
                match node {
                    FragmentNodeRef::Inline(node) => {
                        **node = json!({"t": "RawInline", "c": ["html", &html]});
                    }
                    FragmentNodeRef::Block(node) => {
                        **node = json!({"t": "RawBlock", "c": ["html", &html]});
                    }
                }
            }
        }

        let lzma_options = LzmaOptions::new_preset(9)?;
        let mut decompress_script = String::new();
        let svg_data = if self.config.optimizer.enabled {
            svgs.iter()
                .map(|tree| -> Result<Cow<[u8]>> {
                    Ok(Cow::Owned(svg_optimize::optimize(
                        tree,
                        self.config.optimizer.eps,
                    )?))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            svg_data.iter().map(|data| Cow::Borrowed(*data)).collect()
        };
        for (i, (svg, class_name)) in svg_data.into_iter().zip(svg_class_names).enumerate() {
            let start = Instant::now();
            let original_size = svg.len();
            let mut svg_compressor = XzEncoder::new_stream(
                Cursor::new(svg),
                xz2::stream::Stream::new_lzma_encoder(&lzma_options)?,
            );
            let mut svg_compressed = vec![];
            svg_compressor.read_to_end(&mut svg_compressed)?;
            let svg_encoded = base64::encode(svg_compressed);
            decompress_script.push_str(&formatdoc!(
                r##"
                    var w{page}=new Worker(s);
                    w{page}.onmessage=f("{class_name}");
                    w{page}.postMessage("{svg}");
                "##,
                page = i + 1,
                svg = svg_encoded,
                class_name = class_name
            ));

            eprintln!(
                "SVG for page {} compressed from {} down to {} (base64 encoded) in {}s",
                i + 1,
                ByteSize::b(original_size as u64),
                ByteSize::b(svg_encoded.len() as u64),
                start.elapsed().as_secs_f64()
            );
        }

        let final_code = formatdoc!(
            r##"
            <script {extra_attribs}>
                (function(){{
                    var s=URL.createObjectURL(new Blob(['"function"==typeof importScripts&&(importScripts("{lzma_js_path}"),onmessage=function(a){{LZMA.decompress(Uint8Array.from(atob(a.data),function(a){{return a.charCodeAt(0)}}),function(a,b){{postMessage(a)}})}})'], {{type: "text/javascript"}}));
                    var f=function(a){{return function(e){{for(var f=URL.createObjectURL(new Blob([typeof e.data==="string"?e.data:new Uint8Array(e.data)],{{type:"image/svg+xml"}})),c=document.getElementsByClassName(a),b=0;b<c.length;b++){{var d=c[b].src.indexOf("#");-1!=d&&(c[b].src=f+c[b].src.substring(d))}}}}}};
                    {decompress_script}
                }}());
            </script>
            "##,
            extra_attribs = self.config.script_extra_attributes,
            lzma_js_path = self.config.lzma_js_path,
            decompress_script = decompress_script
        );
        *final_node = json!({
            "t": "RawBlock",
            "c": [
                "html",
                final_code,
            ]
        });
        Ok(())
    }

    // Below are a lot of tree-walking methods.
    // I wasn't aware of any good libraries for parsing Pandoc ASTs when I wrote all of these. And
    // by the time I knew pandoc-ast or pandoc-types I realized I reinvented the wheels again.
    // That said now that I think of it again, there's something JustLaTeX needs that pandoc-ast
    // does not yet offer: after visiting every math node we need to keep a series of mut references
    // to the math nodes so we can change them to inline svgs later. Pandoc-ast's MutVisitor traits
    // does saves a ton of the boilerplates below but the trait methods do not have lifetime
    // parameters, making it impossible to store references for future use safely. Hopefully this
    // justifies a ton of unwieldly practices below...

    /// Walks the tree and look for math nodes. Also creates and returns the reference to an empty
    /// final node, which we will modify later. Due to the borrow checker this is the only place we
    /// can add stuff to the tree. If we just call self.walk_blocks(&mut tree["blocks"], "Document")
    /// in render_with_latex() and try to modify tree["blocks"] afterwards, the borrow checker will
    /// complain.
    fn walk_and_create_final_node(&mut self, tree: &'a mut Value) -> Result<&'a mut Value> {
        let blocks = tree["blocks"]
            .as_array_mut()
            .context("reading [blocks] of the Document")?;
        let last_idx = blocks.len();
        blocks.push(json!({}));
        let mut ret = None;
        for (i, block) in blocks.iter_mut().enumerate() {
            if i == last_idx {
                ret = Some(block);
            } else {
                self.walk_block(block, Style::Plain)?;
            }
        }
        Ok(ret.unwrap())
    }

    fn walk_block(&mut self, value: &'a mut Value, style: Style) -> Result<()> {
        match value["t"].as_str().context("reading type of Block")? {
            "Para" => self.walk_inlines(&mut value["c"], "Para", style),
            "Plain" => self.walk_inlines(&mut value["c"], "Plain", style),
            "LineBlock" => self.walk_list_of_inlines(&mut value["c"], "LineBlock", style),
            "Header" => {
                let level = value["c"][0].as_u64().context("reading level of Header")?;
                self.walk_inlines(
                    &mut value["c"][2],
                    "Header",
                    style.push(StyleElement::Header(level)),
                )?;
                Ok(())
            }
            "BlockQuote" => self.walk_blocks(
                &mut value["c"],
                "BlockQuote",
                style.push(StyleElement::Quote),
            ),
            "OrderedList" => self.walk_list_of_blocks(&mut value["c"][1], "OrderedList", style),
            "BulletList" => self.walk_list_of_blocks(&mut value["c"], "BulletList", style),
            "Div" => self.walk_list_of_blocks(&mut value["c"][1], "Div", style),
            "RawBlock" => {
                let c = &value["c"];
                let format = c[0].as_str().context("reading format of RawBlock")?;
                if format == "tex" {
                    let text = String::from(c[1].as_str().context("reading source of RawBlock")?);
                    self.add_fragment(
                        if text.trim_start().starts_with("%dontshow") {
                            FragmentType::DontShow
                        } else {
                            FragmentType::RawBlock
                        },
                        &text,
                        FragmentNodeRef::Block(value),
                    );
                }
                Ok(())
            }
            "Table" => {
                for (i, content) in value["c"]
                    .as_array_mut()
                    .context("reading contents of Table")?
                    .iter_mut()
                    .enumerate()
                // Circumvent the borrow checker ... isn't it nasty?
                {
                    match i {
                        1 => {
                            self.walk_blocks(&mut content[1], "Table.Caption", style.clone())?;
                        }
                        3 => {
                            self.walk_rows(&mut content[1], "Table.TableHead", style.clone())?;
                        }
                        4 => {
                            for table_body in content
                                .as_array_mut()
                                .context("reading Table.[TableBody]")?
                            {
                                for rows in table_body
                                    .as_array_mut()
                                    .context("reading content of Table.[TableBody]")?
                                    .iter_mut()
                                    .skip(2)
                                {
                                    self.walk_rows(rows, "Table.[TableBody].[Row]", style.clone())?;
                                }
                            }
                        }
                        5 => {
                            self.walk_rows(&mut content[1], "Table.TableFoot", style.clone())?;
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn walk_inline(&mut self, value: &'a mut Value, style: Style) -> Result<()> {
        match value["t"].as_str().context("reading type of Inline")? {
            "Math" => {
                let c = &value["c"];
                let ty = c[0]["t"].as_str().context("reading type of Math")?;
                let text = String::from(c[1].as_str().context("reading source of Math")?);
                let ty = match ty {
                    // A better idea would be to use persistent list which avoids cloning and much
                    // of the push-and-pop boilerplates below. But empirically style don't have
                    // a lot of elements.
                    "InlineMath" => FragmentType::InlineMath(style),
                    "DisplayMath" => {
                        let trimmed_text = text.trim_start();
                        if trimmed_text.starts_with("%raw") {
                            FragmentType::RawBlock
                        } else if trimmed_text.starts_with("%dontshow") {
                            FragmentType::DontShow
                        } else {
                            FragmentType::DisplayMath
                        }
                    }
                    _ => bail!("unknown math type {}", ty),
                };
                self.add_fragment(ty, &text, FragmentNodeRef::Inline(value));
                Ok(())
            }
            "Emph" => self.walk_inlines(&mut value["c"], "Emph", style.push(StyleElement::Emph)),
            // TODO: render them differently in latex.
            "Underline" => self.walk_inlines(&mut value["c"], "Underline", style),
            "Strong" => {
                self.walk_inlines(&mut value["c"], "Strong", style.push(StyleElement::Strong))
            }
            "Strikeout" => self.walk_inlines(&mut value["c"], "Strikeout", style),
            "Link" => self.walk_inlines(&mut value["c"][1], "Link", style),
            "Image" => self.walk_inlines(&mut value["c"][1], "Image", style),
            _ => Ok(()),
        }
    }

    fn walk_blocks(&mut self, value: &'a mut Value, parent: &str, style: Style) -> Result<()> {
        for block in value
            .as_array_mut()
            .with_context(|| format!("reading {}.[Block]", parent))?
            .iter_mut()
        {
            self.walk_block(block, style.clone())?;
        }
        Ok(())
    }

    fn walk_list_of_blocks(
        &mut self,
        value: &'a mut Value,
        parent: &str,
        style: Style,
    ) -> Result<()> {
        for blocks in value
            .as_array_mut()
            .with_context(|| format!("reading {}.[[Block]]", parent))?
            .iter_mut()
        {
            self.walk_blocks(blocks, parent, style.clone())?;
        }
        Ok(())
    }

    fn walk_inlines(&mut self, value: &'a mut Value, parent: &str, style: Style) -> Result<()> {
        for inline in value
            .as_array_mut()
            .with_context(|| format!("reading {}.[Inline]", parent))?
            .iter_mut()
        {
            self.walk_inline(inline, style.clone())?;
        }
        Ok(())
    }

    fn walk_list_of_inlines(
        &mut self,
        value: &'a mut Value,
        parent: &str,
        style: Style,
    ) -> Result<()> {
        for inlines in value
            .as_array_mut()
            .with_context(|| format!("reading {}.[[Inline]]", parent))?
            .iter_mut()
        {
            self.walk_inlines(inlines, parent, style.clone())?;
        }
        Ok(())
    }

    fn walk_rows(&mut self, value: &'a mut Value, parent: &str, style: Style) -> Result<()> {
        for row in value
            .as_array_mut()
            .with_context(|| format!("reading {}.[Row]", parent))?
        {
            for cell in row[1]
                .as_array_mut()
                .with_context(|| format!("reading {}.[Row].[Cell]", parent))?
            {
                self.walk_blocks(
                    &mut cell[4],
                    "[Cell] of Row of TableHead of Table",
                    style.clone(),
                )?;
            }
        }
        Ok(())
    }
}
