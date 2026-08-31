//! Project Hub state and UI. This crate deliberately has no GPU or filesystem access.
mod editor;

pub use editor::{
    ActivePreviewDecoderBackend, ActivePreviewDiagnostic, ActivePreviewFallbackReason,
    ActivePreviewSourceKind, AudioPlaybackTransitionEnvelope, AudioPlaybackTransitionRole,
    CachedWaveform, DEFAULT_STILL_IMAGE_DURATION, EDITOR_PROJECT_SNAPSHOT_VERSION, EditorAction,
    EditorExportStatus, EditorMediaSnapshot, EditorProjectSnapshot, EditorRestoreError,
    EditorState, EditorViewSnapshot, EditorWorkspace, EguiTimelineCanvas, EguiViewerCanvas,
    InvalidProjectFrameRate, LIVE_PIPELINE_TIMING_STAGE_COUNT, LivePipelineTiming,
    LivePipelineTimingRepresentative, LivePipelineTimingSample, LivePipelineTimingStage, MediaId,
    MediaKind, MediaMetadata, MediaStreamMetadata, MonitorFrame, PREVIEW_VIDEO_LAYER_COUNT,
    PreviewQuality, PreviewSampling, ProjectFrameRate, ProxyMediaStatus, RuntimeDiagnostics,
    SourceFrameRate, SourceFrameTimeIndex, TimelineCanvas, TimelineFlag, TimelineMarker,
    TimelineScrubGeometry, TimelineTool, TimelineTrackDensity, TrackHeightSnapshot,
    VideoStripLayout, ViewerCanvas, classify_path, show_editor, show_editor_with_canvases,
    show_editor_with_timeline_canvas,
};

use egui::{
    Align, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, Order, Pos2,
    Rect, RichText, Sense, Stroke, TextStyle, Vec2,
};

pub use nle_title::NOTO_SANS_JP;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    Japanese,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Library,
    Templates,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    List,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Recent,
    Name,
    Size,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateId {
    Hd720p,
    FullHd1080p,
    Uhd2160p4k,
    Uhd8k,
    VerticalHd720p,
    VerticalFullHdSocial,
    Vertical4kMaster,
    PodcastStudio,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateCategory {
    Landscape,
    Vertical,
    Audio,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoDimensions {
    pub width: u32,
    pub height: u32,
    pub aspect: &'static str,
    pub fps: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct TemplatePreset {
    pub id: TemplateId,
    pub category: TemplateCategory,
    pub video: Option<VideoDimensions>,
    english_name: &'static str,
    japanese_name: &'static str,
    english_description: &'static str,
    japanese_description: &'static str,
    english_platforms: &'static str,
    japanese_platforms: &'static str,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub id: u32,
    pub name: String,
    pub recent: String,
    pub size: String,
    /// Runtime-only hub preview supplied by the application layer. It is never catalog data.
    pub thumbnail: Option<egui::TextureId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubAction {
    NewProject {
        name: String,
        template: TemplateId,
        language: Language,
    },
    OpenProject {
        language: Language,
    },
    OpenExisting {
        project_id: u32,
        language: Language,
    },
    Import {
        language: Language,
    },
    Export {
        project_id: u32,
        language: Language,
    },
    Duplicate {
        project_id: u32,
        language: Language,
    },
}
#[derive(Clone, Debug)]
pub enum Dialog {
    NewProject { name: String, template: TemplateId },
    NewFolder { name: String },
}
#[derive(Clone, Debug)]
pub struct ProjectHubState {
    pub language: Language,
    pub page: Page,
    pub view: ViewMode,
    pub sort: SortMode,
    pub search: String,
    pub thumbnail_scale: f32,
    pub selected: Option<u32>,
    pub projects: Vec<Project>,
    pub collections: Vec<String>,
    pub dialog: Option<Dialog>,
    pub status: Option<String>,
    action: Option<HubAction>,
}
impl Default for ProjectHubState {
    fn default() -> Self {
        Self::new(false)
    }
}
impl ProjectHubState {
    pub fn new(demo: bool) -> Self {
        Self {
            language: Language::English,
            page: Page::Library,
            view: ViewMode::Grid,
            sort: SortMode::Recent,
            search: String::new(),
            thumbnail_scale: 1.0,
            selected: None,
            projects: if demo { demo_projects() } else { vec![] },
            collections: vec![],
            dialog: None,
            status: None,
            action: None,
        }
    }
    /// Replaces the catalog-owned project cards without changing hub preferences.
    pub fn set_projects(&mut self, projects: Vec<Project>) {
        self.projects = projects;
        self.selected = None;
    }
    pub fn set_thumbnail_scale(&mut self, v: f32) {
        self.thumbnail_scale = v.clamp(0.75, 1.5)
    }
    pub fn visible_projects(&self) -> Vec<&Project> {
        let query = self.search.to_lowercase();
        let mut projects: Vec<_> = self
            .projects
            .iter()
            .filter(|p| p.name.to_lowercase().contains(&query))
            .collect();
        projects.sort_by(|a, b| match self.sort {
            SortMode::Recent => a.id.cmp(&b.id),
            SortMode::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortMode::Size => size_value(&b.size).cmp(&size_value(&a.size)),
        });
        projects
    }
    pub fn take_action(&mut self) -> Option<HubAction> {
        self.action.take()
    }
    fn emit(&mut self, a: HubAction) {
        self.status = Some(tr(
            self.language,
            "Project I/O will be connected in a later build.",
            "プロジェクト I/O は後のビルドで接続されます。",
        ));
        self.action = Some(a)
    }
}
fn size_value(s: &str) -> u64 {
    let mut p = s.split_whitespace();
    let v = p.next().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.);
    if p.next() == Some("GB") {
        (v * 1024.) as u64
    } else {
        v as u64
    }
}
fn demo_projects() -> Vec<Project> {
    [
        (1, "Winter Session", "Just now", "842 MB"),
        (2, "Harbor Cut", "Yesterday", "1.6 GB"),
        (3, "Glass Letters", "3 days ago", "204 MB"),
        (4, "Northbound", "Last week", "3.2 GB"),
    ]
    .into_iter()
    .map(|(id, name, recent, size)| Project {
        id,
        name: name.into(),
        recent: recent.into(),
        size: size.into(),
        thumbnail: None,
    })
    .collect()
}
fn tr(l: Language, en: &str, jp: &str) -> String {
    match l {
        Language::English => en,
        Language::Japanese => jp,
    }
    .into()
}
pub fn configure_fonts(fonts: &mut FontDefinitions) {
    fonts.font_data.insert(
        "noto-sans-jp".into(),
        FontData::from_static(NOTO_SANS_JP).into(),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "noto-sans-jp".into())
}

pub fn show(ui: &mut egui::Ui, s: &mut ProjectHubState) {
    show_with_backdrops(ui, s, None);
}

#[derive(Clone, Copy, Debug)]
pub struct HubBackdrops {
    pub english: egui::TextureId,
    pub japanese: egui::TextureId,
    pub image_size: Vec2,
}

impl HubBackdrops {
    fn selected(self, language: Language) -> egui::TextureId {
        match language {
            Language::English => self.english,
            Language::Japanese => self.japanese,
        }
    }
}

pub fn show_with_backdrops(
    ui: &mut egui::Ui,
    s: &mut ProjectHubState,
    backdrops: Option<HubBackdrops>,
) {
    let ctx = ui.ctx().clone();
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals.panel_fill = Color32::from_rgb(13, 16, 21);
        style.visuals.window_fill = Color32::from_rgb(23, 28, 36);
        style.visuals.selection.bg_fill = Color32::from_rgb(34, 80, 115);
        style.visuals.override_text_color = Some(Color32::from_rgb(210, 218, 228));
        style.visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(192, 202, 214);
        style.visuals.widgets.inactive.fg_stroke.color = Color32::from_rgb(184, 195, 207);
        style.visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(28, 39, 50);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(32, 62, 82);
        style.spacing.item_spacing = Vec2::new(9.0, 8.0);
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(25.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        );
    });
    header_controls(&ctx, s);
    egui::Panel::left("rail").exact_size(210.).show(ui, |ui| {
        ui.add_space(22.);
        ui.label(
            RichText::new("MAELSTROM")
                .size(22.)
                .strong()
                .color(Color32::from_rgb(174, 215, 241)),
        );
        ui.label(
            RichText::new("VIDEO + AUDIO EDITOR")
                .size(9.)
                .color(Color32::GRAY),
        );
        ui.add_space(28.);
        if primary(ui, &tr(s.language, "New Project", "新規プロジェクト"))
            .on_hover_text(tr(
                s.language,
                "Create a local editing project",
                "ローカル編集プロジェクトを作成",
            ))
            .clicked()
        {
            s.dialog = Some(Dialog::NewProject {
                name: String::new(),
                template: TemplateId::FullHd1080p,
            })
        }
        if ui
            .button(tr(s.language, "Open Project", "プロジェクトを開く"))
            .on_hover_text(tr(
                s.language,
                "Choose an existing project from disk",
                "ディスクから既存のプロジェクトを選択",
            ))
            .clicked()
        {
            s.emit(HubAction::OpenProject {
                language: s.language,
            })
        }
        ui.add_space(20.);
        nav(
            ui,
            s,
            Page::Library,
            "▦",
            &tr(s.language, "Library", "ライブラリ"),
        );
        nav(
            ui,
            s,
            Page::Templates,
            "◇",
            &tr(s.language, "Templates", "テンプレート"),
        );
        ui.add_space(18.);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(tr(s.language, "COLLECTIONS", "コレクション"))
                    .small()
                    .color(Color32::GRAY),
            );
            if ui
                .small_button("+")
                .on_hover_text(tr(s.language, "New Folder", "新しいフォルダー"))
                .clicked()
            {
                s.dialog = Some(Dialog::NewFolder {
                    name: String::new(),
                })
            }
        });
        for folder in &s.collections {
            ui.label(format!("  {folder}"));
        }
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.label(
                RichText::new(tr(s.language, "LOCAL ONLY", "ローカルのみ"))
                    .small()
                    .color(Color32::GRAY),
            );
        });
    });
    egui::CentralPanel::default().show(ui, |ui| {
        if let Some(backdrops) = backdrops {
            paint_backdrop(ui, backdrops, s.language);
        }
        ui.add_space(18.);
        ui.heading(tr(s.language, "Projects", "プロジェクト"));
        ui.label(
            RichText::new(tr(
                s.language,
                "Your local editing workspace",
                "ローカル編集ワークスペース",
            ))
            .color(Color32::GRAY),
        );
        ui.add_space(70.);
        match s.page {
            Page::Library => library(ui, s, backdrops),
            Page::Templates => templates(ui, s),
        }
        if let Some(status) = &s.status {
            ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                ui.separator();
                ui.label(RichText::new(status).small().color(Color32::LIGHT_BLUE));
            });
        }
    });
    dialog(&ctx, s)
}

fn paint_backdrop(ui: &egui::Ui, backdrops: HubBackdrops, language: Language) {
    let bounds = ui.max_rect().shrink2(Vec2::new(28.0, 46.0));
    let scale =
        (bounds.width() / backdrops.image_size.x).min(bounds.height() / backdrops.image_size.y);
    let image_rect = egui::Rect::from_center_size(bounds.center(), backdrops.image_size * scale);
    ui.painter().image(
        backdrops.selected(language),
        image_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
        Color32::from_white_alpha(20),
    );
}

fn header_controls(ctx: &egui::Context, s: &mut ProjectHubState) {
    egui::Area::new(egui::Id::new("project-hub-header-controls"))
        .anchor(Align2::RIGHT_TOP, Vec2::new(-18.0, 18.0))
        .order(Order::Foreground)
        .movable(false)
        .show(ctx, |ui| {
            ui.set_width(245.0);
            language(ui, s);
            ui.add_space(6.0);
            ui.add_sized(
                [245.0, 30.0],
                egui::TextEdit::singleline(&mut s.search).hint_text(tr(
                    s.language,
                    "Search projects",
                    "プロジェクトを検索",
                )),
            );
        });
}

fn primary(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [180., 34.],
        egui::Button::new(RichText::new(label).color(Color32::WHITE))
            .fill(Color32::from_rgb(28, 69, 101)),
    )
}
fn nav(ui: &mut egui::Ui, s: &mut ProjectHubState, page: Page, icon: &str, label: &str) {
    if ui
        .selectable_label(s.page == page, format!("{icon}  {label}"))
        .clicked()
    {
        s.page = page
    }
}
fn language(ui: &mut egui::Ui, s: &mut ProjectHubState) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new("Language / 言語")
                .small()
                .color(Color32::from_rgb(137, 151, 165)),
        );
        ui.allocate_ui_with_layout(
            Vec2::new(210.0, 30.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                language_option(ui, s, Language::English, "EN  English");
                language_option(ui, s, Language::Japanese, "JP  日本語");
            },
        );
    });
}

fn language_option(ui: &mut egui::Ui, s: &mut ProjectHubState, language: Language, label: &str) {
    let selected = s.language == language;
    let button = egui::Button::new(RichText::new(label).color(if selected {
        Color32::from_rgb(235, 246, 255)
    } else {
        Color32::from_rgb(155, 168, 181)
    }))
    .fill(if selected {
        Color32::from_rgb(28, 55, 75)
    } else {
        Color32::TRANSPARENT
    })
    .stroke(Stroke::new(
        1.0,
        if selected {
            Color32::from_rgb(91, 180, 232)
        } else {
            Color32::TRANSPARENT
        },
    ));
    if ui.add_sized([102.0, 28.0], button).clicked() {
        s.language = language;
    }
}
fn library(ui: &mut egui::Ui, s: &mut ProjectHubState, backdrops: Option<HubBackdrops>) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{} {}",
                s.visible_projects().len(),
                tr(s.language, "projects", "プロジェクト")
            ))
            .color(Color32::GRAY),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.selectable_value(&mut s.view, ViewMode::List, "☷")
                .on_hover_text(tr(s.language, "List view", "リスト表示"));
            ui.selectable_value(&mut s.view, ViewMode::Grid, "▦")
                .on_hover_text(tr(s.language, "Thumbnail grid", "サムネイルグリッド"));
            if ui
                .button(tr(s.language, "Import", "インポート"))
                .on_hover_text(tr(
                    s.language,
                    "Import a project from disk",
                    "ディスクからプロジェクトをインポート",
                ))
                .clicked()
            {
                s.emit(HubAction::Import {
                    language: s.language,
                });
            }
            egui::ComboBox::from_id_salt("sort")
                .selected_text(match s.sort {
                    SortMode::Recent => tr(s.language, "Recent", "最近"),
                    SortMode::Name => tr(s.language, "Name", "名前"),
                    SortMode::Size => tr(s.language, "Size", "サイズ"),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut s.sort,
                        SortMode::Recent,
                        tr(s.language, "Recent", "最近"),
                    );
                    ui.selectable_value(
                        &mut s.sort,
                        SortMode::Name,
                        tr(s.language, "Name", "名前"),
                    );
                    ui.selectable_value(
                        &mut s.sort,
                        SortMode::Size,
                        tr(s.language, "Size", "サイズ"),
                    );
                })
                .response
                .on_hover_text(tr(
                    s.language,
                    "Choose how projects are sorted",
                    "プロジェクトの並び順を選択",
                ));
            if s.view == ViewMode::Grid {
                ui.add(egui::Slider::new(&mut s.thumbnail_scale, 0.75..=1.5).show_value(false))
                    .on_hover_text(tr(s.language, "Thumbnail size", "サムネイルサイズ"));
            }
        });
    });
    ui.separator();
    if s.visible_projects().is_empty() {
        ui.add_space(90.);
        ui.vertical_centered(|ui| {
            ui.heading(tr(
                s.language,
                "Your library is ready.",
                "ライブラリの準備ができました。",
            ));
            ui.label(tr(
                s.language,
                "Create a project, open one later, or start from a template.",
                "プロジェクトを作成するか、後で開くか、テンプレートから始めます。",
            ));
            ui.add_space(10.);
            if primary(ui, &tr(s.language, "New Project", "新規プロジェクト"))
                .on_hover_text(tr(
                    s.language,
                    "Create a local editing project",
                    "ローカル編集プロジェクトを作成",
                ))
                .clicked()
            {
                s.dialog = Some(Dialog::NewProject {
                    name: String::new(),
                    template: TemplateId::FullHd1080p,
                })
            }
        });
    } else if s.view == ViewMode::Grid {
        grid(ui, s, backdrops)
    } else {
        list(ui, s)
    }
}
fn select(s: &mut ProjectHubState, p: &Project, r: &egui::Response) {
    if r.clicked() {
        s.selected = Some(p.id)
    }
    if r.double_clicked() {
        s.emit(HubAction::OpenExisting {
            project_id: p.id,
            language: s.language,
        })
    }
}
fn grid(ui: &mut egui::Ui, s: &mut ProjectHubState, backdrops: Option<HubBackdrops>) {
    let ps: Vec<_> = s.visible_projects().into_iter().cloned().collect();
    let w = 190.0 * s.thumbnail_scale;
    let columns = ((ui.available_width() + 10.0) / (w + 10.0))
        .floor()
        .max(1.0) as usize;
    egui::Grid::new("project-card-grid")
        .num_columns(columns)
        .min_col_width(0.0)
        .spacing(Vec2::new(10.0, 10.0))
        .show(ui, |ui| {
            for (index, p) in ps.iter().enumerate() {
                let selected = s.selected == Some(p.id);
                let r = egui::Frame::group(ui.style())
                    .fill(if selected {
                        Color32::from_rgb(27, 53, 73)
                    } else {
                        Color32::from_rgb(25, 30, 38)
                    })
                    .stroke(Stroke::new(
                        1.,
                        if selected {
                            Color32::from_rgb(104, 177, 219)
                        } else {
                            Color32::from_rgb(49, 57, 68)
                        },
                    ))
                    .show(ui, |ui| {
                        ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                            let height = w * 0.52 + 52.0;
                            ui.set_min_width(w);
                            ui.set_max_width(w);
                            ui.set_min_height(height);
                            ui.set_max_height(height);
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::new(w, w * 0.52), Sense::hover());
                            let paint = ui.painter();
                            if let Some(texture) = p.thumbnail {
                                paint.image(
                                    texture,
                                    rect,
                                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            } else if let Some(backdrops) = backdrops {
                                paint.image(
                                    backdrops.selected(s.language),
                                    rect,
                                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            } else {
                                paint.rect_filled(rect, 6., Color32::from_rgb(10, 24, 37));
                            }
                            ui.label(RichText::new(&p.name).strong());
                            ui.label(
                                RichText::new(format!("{} · {}", p.recent, p.size))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        });
                    })
                    .response
                    .interact(Sense::click())
                    .on_hover_text(tr(
                        s.language,
                        "Select this project; double-click to open",
                        "選択するにはクリック、開くにはダブルクリック",
                    ));
                select(s, p, &r);
                if (index + 1) % columns == 0 {
                    ui.end_row();
                }
            }
        });
    actions(ui, s)
}
fn list(ui: &mut egui::Ui, s: &mut ProjectHubState) {
    let ps: Vec<_> = s.visible_projects().into_iter().cloned().collect();
    let spacing = ui.spacing().item_spacing.x;
    let [name_width, recent_width, size_width] = list_column_widths(ui.available_width(), spacing);
    let row_height = 32.0;
    egui::Grid::new("project-list")
        .striped(true)
        .show(ui, |ui| {
            for (heading, width) in [
                (tr(s.language, "Name", "名前"), name_width),
                (tr(s.language, "Last opened", "最終オープン"), recent_width),
                (tr(s.language, "Size", "サイズ"), size_width),
            ] {
                ui.add_sized(
                    [width, 24.0],
                    egui::Label::new(RichText::new(heading).small().color(Color32::GRAY)),
                );
            }
            ui.end_row();
            for p in &ps {
                let r = ui.add_sized(
                    [name_width, row_height],
                    egui::Button::selectable(s.selected == Some(p.id), &p.name),
                );
                select(s, p, &r);
                ui.add_sized(
                    [recent_width, row_height],
                    egui::Label::new(&p.recent).truncate(),
                );
                ui.add_sized(
                    [size_width, row_height],
                    egui::Label::new(&p.size).truncate(),
                );
                ui.end_row();
            }
        });
    actions(ui, s)
}

fn list_column_widths(available_width: f32, spacing: f32) -> [f32; 3] {
    let usable = (available_width - spacing * 2.0).max(360.0);
    [usable * 0.48, usable * 0.32, usable * 0.20]
}
fn actions(ui: &mut egui::Ui, s: &mut ProjectHubState) {
    if let Some(id) = s.selected {
        ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(tr(s.language, "Open", "開く"))
                    .on_hover_text(tr(
                        s.language,
                        "Open selected project",
                        "選択したプロジェクトを開く",
                    ))
                    .clicked()
                {
                    s.emit(HubAction::OpenExisting {
                        project_id: id,
                        language: s.language,
                    })
                }
                if ui
                    .button(tr(s.language, "Export", "エクスポート"))
                    .on_hover_text(tr(
                        s.language,
                        "Export selected project",
                        "選択したプロジェクトをエクスポート",
                    ))
                    .clicked()
                {
                    s.emit(HubAction::Export {
                        project_id: id,
                        language: s.language,
                    })
                }
                if ui
                    .button(tr(s.language, "Duplicate", "複製"))
                    .on_hover_text(tr(
                        s.language,
                        "Copy selected project",
                        "選択したプロジェクトをコピー",
                    ))
                    .clicked()
                {
                    s.emit(HubAction::Duplicate {
                        project_id: id,
                        language: s.language,
                    })
                }
            })
        });
    }
}
fn templates(ui: &mut egui::Ui, s: &mut ProjectHubState) {
    ui.label(
        RichText::new(tr(
            s.language,
            "Start from a template",
            "テンプレートから始める",
        ))
        .color(Color32::GRAY),
    );
    ui.add_space(12.);
    for category in [
        TemplateCategory::Landscape,
        TemplateCategory::Vertical,
        TemplateCategory::Audio,
    ] {
        ui.add_space(8.);
        ui.label(RichText::new(category_text(s.language, category)).strong());
        template_grid(ui, s, category);
    }
}
fn template_grid(ui: &mut egui::Ui, s: &mut ProjectHubState, category: TemplateCategory) {
    let card_width = 220.0;
    let columns = ((ui.available_width() + 10.0) / (card_width + 10.0))
        .floor()
        .max(1.0) as usize;
    let presets: Vec<_> = templates_data()
        .into_iter()
        .filter(|preset| preset.category == category)
        .collect();
    egui::Grid::new(format!("template-card-grid-{category:?}"))
        .num_columns(columns)
        .min_col_width(0.0)
        .spacing(Vec2::new(10.0, 10.0))
        .show(ui, |ui| {
            for (index, preset) in presets.into_iter().enumerate() {
                let r = egui::Frame::group(ui.style())
                    .show(ui, |ui| {
                        ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                            ui.set_min_width(card_width);
                            ui.set_max_width(card_width);
                            ui.set_min_height(142.0);
                            ui.set_max_height(142.0);
                            ui.horizontal(|ui| {
                                template_format_icon(ui, &preset);
                                ui.heading(template_name(s.language, &preset));
                            });
                            ui.label(
                                RichText::new(template_description(s.language, &preset))
                                    .small()
                                    .color(Color32::GRAY),
                            );
                            if let Some(video) = preset.video {
                                ui.label(
                                    RichText::new(format!(
                                        "{} × {}  ·  {}  ·  {} fps",
                                        video.width, video.height, video.aspect, video.fps
                                    ))
                                    .small()
                                    .color(Color32::from_rgb(151, 194, 221)),
                                );
                            }
                            ui.label(
                                RichText::new(template_platforms(s.language, &preset))
                                    .small()
                                    .color(Color32::from_rgb(137, 151, 165)),
                            );
                        });
                    })
                    .response
                    .interact(Sense::click())
                    .on_hover_text(tr(
                        s.language,
                        "Start a new project with this template",
                        "このテンプレートで新しいプロジェクトを開始",
                    ));
                if r.clicked() {
                    s.dialog = Some(Dialog::NewProject {
                        name: String::new(),
                        template: preset.id,
                    })
                }
                if (index + 1) % columns == 0 {
                    ui.end_row();
                }
            }
        });
}

fn template_format_icon(ui: &mut egui::Ui, preset: &TemplatePreset) {
    let (slot, _) = ui.allocate_exact_size(Vec2::new(42.0, 28.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(slot, 3.0, Color32::from_rgb(8, 17, 25));

    if let Some(video) = preset.video {
        let aspect = video.width as f32 / video.height as f32;
        let (width, height) = if aspect >= 1.0 {
            (36.0, 36.0 / aspect)
        } else {
            (22.0 * aspect, 22.0)
        };
        let frame = Rect::from_center_size(slot.center(), Vec2::new(width, height));
        painter.rect_filled(frame, 1.5, Color32::from_rgb(18, 53, 78));
        let color = Color32::from_rgb(112, 190, 235);
        painter.line_segment(
            [frame.left_top(), frame.right_top()],
            Stroke::new(1.0, color),
        );
        painter.line_segment(
            [frame.right_top(), frame.right_bottom()],
            Stroke::new(1.0, color),
        );
        painter.line_segment(
            [frame.right_bottom(), frame.left_bottom()],
            Stroke::new(1.0, color),
        );
        painter.line_segment(
            [frame.left_bottom(), frame.left_top()],
            Stroke::new(1.0, color),
        );
    } else {
        let color = Color32::from_rgb(112, 190, 235);
        for (index, height) in [6.0, 14.0, 9.0, 18.0, 11.0, 7.0].into_iter().enumerate() {
            let x = slot.left() + 7.0 + index as f32 * 5.5;
            painter.line_segment(
                [
                    Pos2::new(x, slot.center().y - height * 0.5),
                    Pos2::new(x, slot.center().y + height * 0.5),
                ],
                Stroke::new(1.5, color),
            );
        }
    }
}

pub fn template_video_dimensions(id: TemplateId) -> Option<VideoDimensions> {
    templates_data()
        .into_iter()
        .find(|preset| preset.id == id)
        .and_then(|preset| preset.video)
}

fn templates_data() -> [TemplatePreset; 8] {
    [
        TemplatePreset {
            id: TemplateId::Hd720p,
            category: TemplateCategory::Landscape,
            video: Some(VideoDimensions {
                width: 1280,
                height: 720,
                aspect: "16:9",
                fps: 30,
            }),
            english_name: "HD 720p",
            japanese_name: "HD 720p",
            english_description: "A lightweight landscape edit.",
            japanese_description: "軽量な横長編集。",
            english_platforms: "YouTube & web",
            japanese_platforms: "YouTube・Web",
        },
        TemplatePreset {
            id: TemplateId::FullHd1080p,
            category: TemplateCategory::Landscape,
            video: Some(VideoDimensions {
                width: 1920,
                height: 1080,
                aspect: "16:9",
                fps: 30,
            }),
            english_name: "Full HD 1080p",
            japanese_name: "フル HD 1080p",
            english_description: "Standard landscape production.",
            japanese_description: "標準的な横長制作。",
            english_platforms: "YouTube & web",
            japanese_platforms: "YouTube・Web",
        },
        TemplatePreset {
            id: TemplateId::Uhd2160p4k,
            category: TemplateCategory::Landscape,
            video: Some(VideoDimensions {
                width: 3840,
                height: 2160,
                aspect: "16:9",
                fps: 30,
            }),
            english_name: "UHD 2160p / 4K",
            japanese_name: "UHD 2160p / 4K",
            english_description: "One shared UHD and 4K landscape preset.",
            japanese_description: "UHD と 4K 共通の横長プリセット。",
            english_platforms: "YouTube & web",
            japanese_platforms: "YouTube・Web",
        },
        TemplatePreset {
            id: TemplateId::Uhd8k,
            category: TemplateCategory::Landscape,
            video: Some(VideoDimensions {
                width: 7680,
                height: 4320,
                aspect: "16:9",
                fps: 30,
            }),
            english_name: "8K UHD",
            japanese_name: "8K UHD",
            english_description: "High-resolution landscape master.",
            japanese_description: "高解像度の横長マスター。",
            english_platforms: "Master delivery",
            japanese_platforms: "マスター納品",
        },
        TemplatePreset {
            id: TemplateId::VerticalHd720p,
            category: TemplateCategory::Vertical,
            video: Some(VideoDimensions {
                width: 720,
                height: 1280,
                aspect: "9:16",
                fps: 30,
            }),
            english_name: "Vertical HD 720p",
            japanese_name: "縦型 HD 720p",
            english_description: "A compact vertical edit.",
            japanese_description: "コンパクトな縦型編集。",
            english_platforms: "Mobile video",
            japanese_platforms: "モバイル動画",
        },
        TemplatePreset {
            id: TemplateId::VerticalFullHdSocial,
            category: TemplateCategory::Vertical,
            video: Some(VideoDimensions {
                width: 1080,
                height: 1920,
                aspect: "9:16",
                fps: 30,
            }),
            english_name: "Vertical Full HD",
            japanese_name: "縦型フル HD",
            english_description: "One social preset for vertical publishing.",
            japanese_description: "縦型公開向けの共通ソーシャルプリセット。",
            english_platforms: "YouTube Shorts · TikTok · Instagram Reels · Facebook Reels",
            japanese_platforms: "YouTube ショート・TikTok・Instagram リール・Facebook リール",
        },
        TemplatePreset {
            id: TemplateId::Vertical4kMaster,
            category: TemplateCategory::Vertical,
            video: Some(VideoDimensions {
                width: 2160,
                height: 3840,
                aspect: "9:16",
                fps: 30,
            }),
            english_name: "Vertical 4K Master",
            japanese_name: "縦型 4K マスター",
            english_description: "High-resolution vertical master.",
            japanese_description: "高解像度の縦型マスター。",
            english_platforms: "Mobile master",
            japanese_platforms: "モバイルマスター",
        },
        TemplatePreset {
            id: TemplateId::PodcastStudio,
            category: TemplateCategory::Audio,
            video: None,
            english_name: "Podcast Studio",
            japanese_name: "ポッドキャストスタジオ",
            english_description: "Audio-first session layout.",
            japanese_description: "オーディオ中心のセッションレイアウト。",
            english_platforms: "Podcast & audio",
            japanese_platforms: "ポッドキャスト・音声",
        },
    ]
}
fn template_name(l: Language, preset: &TemplatePreset) -> &'static str {
    match l {
        Language::English => preset.english_name,
        Language::Japanese => preset.japanese_name,
    }
}
fn template_description(l: Language, preset: &TemplatePreset) -> &'static str {
    match l {
        Language::English => preset.english_description,
        Language::Japanese => preset.japanese_description,
    }
}
fn template_platforms(l: Language, preset: &TemplatePreset) -> &'static str {
    match l {
        Language::English => preset.english_platforms,
        Language::Japanese => preset.japanese_platforms,
    }
}
fn category_text(l: Language, category: TemplateCategory) -> String {
    match (l, category) {
        (Language::English, TemplateCategory::Landscape) => "Landscape 16:9",
        (Language::Japanese, TemplateCategory::Landscape) => "横長 16:9",
        (Language::English, TemplateCategory::Vertical) => "Vertical 9:16",
        (Language::Japanese, TemplateCategory::Vertical) => "縦型 9:16",
        (Language::English, TemplateCategory::Audio) => "Audio",
        (Language::Japanese, TemplateCategory::Audio) => "オーディオ",
    }
    .into()
}
fn dialog(ctx: &egui::Context, s: &mut ProjectHubState) {
    let Some(mut d) = s.dialog.take() else { return };
    let mut close = false;
    egui::Window::new(match d {
        Dialog::NewProject { .. } => tr(s.language, "New Project", "新規プロジェクト"),
        Dialog::NewFolder { .. } => tr(s.language, "New Folder", "新しいフォルダー"),
    })
    .collapsible(false)
    .resizable(false)
    .show(ctx, |ui| match &mut d {
        Dialog::NewProject { name, template } => {
            ui.label(tr(s.language, "Project name", "プロジェクト名"));
            let name_response = ui.text_edit_singleline(name);
            let submit_with_enter =
                name_response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.label(tr(s.language, "Template", "テンプレート"));
            egui::ComboBox::from_id_salt("template-select")
                .selected_text(template_name_for_id(s.language, *template))
                .show_ui(ui, |ui| {
                    for preset in templates_data() {
                        ui.selectable_value(
                            template,
                            preset.id,
                            template_name(s.language, &preset),
                        );
                    }
                });
            ui.label(format!(
                "Language / 言語: {}",
                if s.language == Language::English {
                    "English"
                } else {
                    "日本語"
                }
            ));
            let mut submit = submit_with_enter;
            ui.horizontal(|ui| {
                if primary(ui, &tr(s.language, "Create", "作成")).clicked() {
                    submit = true;
                }
                if ui.button(tr(s.language, "Cancel", "キャンセル")).clicked() {
                    close = true
                }
            });
            if submit {
                s.emit(HubAction::NewProject {
                    name: if name.trim().is_empty() {
                        tr(s.language, "Untitled Project", "名称未設定プロジェクト")
                    } else {
                        name.clone()
                    },
                    template: *template,
                    language: s.language,
                });
                close = true;
            }
        }
        Dialog::NewFolder { name } => {
            ui.label(tr(s.language, "Folder name", "フォルダー名"));
            ui.text_edit_singleline(name);
            ui.horizontal(|ui| {
                if primary(ui, &tr(s.language, "Create", "作成")).clicked() {
                    if !name.trim().is_empty() {
                        s.collections.push(name.clone())
                    }
                    close = true
                }
                if ui.button(tr(s.language, "Cancel", "キャンセル")).clicked() {
                    close = true
                }
            });
        }
    });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true
    }
    if !close {
        s.dialog = Some(d)
    }
}
fn template_name_for_id(l: Language, id: TemplateId) -> &'static str {
    templates_data()
        .iter()
        .find(|preset| preset.id == id)
        .map(|preset| template_name(l, preset))
        .expect("every TemplateId has metadata")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translations_exist() {
        for preset in templates_data() {
            assert!(!template_name(Language::English, &preset).is_empty());
            assert!(!template_name(Language::Japanese, &preset).is_empty());
            assert!(!template_description(Language::English, &preset).is_empty());
            assert!(!template_description(Language::Japanese, &preset).is_empty());
            assert!(!template_platforms(Language::English, &preset).is_empty());
            assert!(!template_platforms(Language::Japanese, &preset).is_empty());
        }
    }
    #[test]
    fn action_carries_language() {
        let mut s = ProjectHubState {
            language: Language::Japanese,
            ..Default::default()
        };
        s.emit(HubAction::OpenProject {
            language: s.language,
        });
        assert!(matches!(
            s.take_action(),
            Some(HubAction::OpenProject {
                language: Language::Japanese
            })
        ))
    }
    #[test]
    fn search_is_case_insensitive() {
        let mut s = ProjectHubState::new(true);
        s.search = "hARBOR".into();
        assert_eq!(s.visible_projects().len(), 1)
    }
    #[test]
    fn sorts_work() {
        let mut s = ProjectHubState::new(true);
        s.sort = SortMode::Name;
        assert_eq!(s.visible_projects()[0].name, "Glass Letters");
        s.sort = SortMode::Recent;
        assert_eq!(s.visible_projects()[0].name, "Winter Session");
        s.sort = SortMode::Size;
        assert_eq!(s.visible_projects()[0].name, "Northbound")
    }
    #[test]
    fn thumbnail_clamps() {
        let mut s = ProjectHubState::default();
        s.set_thumbnail_scale(9.);
        assert_eq!(s.thumbnail_scale, 1.5);
        s.set_thumbnail_scale(0.);
        assert_eq!(s.thumbnail_scale, 0.75)
    }

    #[test]
    fn list_columns_fill_the_available_width() {
        let widths = list_column_widths(1_000.0, 8.0);
        assert!((widths.iter().sum::<f32>() + 16.0 - 1_000.0).abs() < 0.001);
        assert!(widths[0] > widths[1]);
        assert!(widths[1] > widths[2]);
    }

    #[test]
    fn template_count_and_ids() {
        let presets = templates_data();
        assert_eq!(presets.len(), 8);
        assert_eq!(
            presets.map(|preset| preset.id),
            [
                TemplateId::Hd720p,
                TemplateId::FullHd1080p,
                TemplateId::Uhd2160p4k,
                TemplateId::Uhd8k,
                TemplateId::VerticalHd720p,
                TemplateId::VerticalFullHdSocial,
                TemplateId::Vertical4kMaster,
                TemplateId::PodcastStudio,
            ]
        );
    }

    #[test]
    fn video_template_sizes_and_categories_are_exact_and_unique() {
        let presets = templates_data();
        assert_eq!(
            presets
                .iter()
                .filter(|p| p.category == TemplateCategory::Landscape)
                .count(),
            4
        );
        assert_eq!(
            presets
                .iter()
                .filter(|p| p.category == TemplateCategory::Vertical)
                .count(),
            3
        );
        assert_eq!(
            presets
                .iter()
                .filter(|p| p.category == TemplateCategory::Audio)
                .count(),
            1
        );
        let videos: Vec<_> = presets.iter().filter_map(|p| p.video).collect();
        assert_eq!(videos.len(), 7);
        assert!(videos.iter().all(|video| video.fps == 30));
        let mut unique = std::collections::BTreeSet::new();
        assert!(
            videos
                .iter()
                .all(|video| unique.insert((video.width, video.height, video.fps)))
        );
        assert!(videos.contains(&VideoDimensions {
            width: 1280,
            height: 720,
            aspect: "16:9",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 1920,
            height: 1080,
            aspect: "16:9",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 3840,
            height: 2160,
            aspect: "16:9",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 7680,
            height: 4320,
            aspect: "16:9",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 720,
            height: 1280,
            aspect: "9:16",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 1080,
            height: 1920,
            aspect: "9:16",
            fps: 30
        }));
        assert!(videos.contains(&VideoDimensions {
            width: 2160,
            height: 3840,
            aspect: "9:16",
            fps: 30
        }));
    }

    #[test]
    fn combined_4k_and_social_labels_are_single_presets() {
        let presets = templates_data();
        let uhd = presets
            .iter()
            .find(|p| p.id == TemplateId::Uhd2160p4k)
            .unwrap();
        assert!(uhd.english_name.contains("2160p / 4K"));
        let social = presets
            .iter()
            .find(|p| p.id == TemplateId::VerticalFullHdSocial)
            .unwrap();
        for platform in [
            "YouTube Shorts",
            "TikTok",
            "Instagram Reels",
            "Facebook Reels",
        ] {
            assert!(social.english_platforms.contains(platform));
        }
    }
}
