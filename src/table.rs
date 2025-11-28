use std::sync::Arc;

use crate::egui::Context;
use csv::StringRecord;
use egui::{Color32, TextFormat};

use crate::types::{ArcSheet, FileHeader, UiMessage};
use egui::scroll_area::ScrollAreaOutput;
use std::sync::mpsc::Sender;

use egui_extras::Table;

// pub fn display_table(
//     ctx: &Context,
//     filename: &str,
//     tab_id: usize,
//     table_ui: Table,
//     filter: &str,
//     columns: &Vec<FileHeader>,
//     promised_data: &Promise<Arc<ArcSheet>>,
//     filtered_data: &Promise<Arc<ArcSheet>>,
//     sender: &Sender<UiMessage>,
// ) -> ScrollAreaOutput<()> {
//     let visible_columns = columns
//         .iter()
//         .enumerate()
//         .filter(|(_, c)| c.visible)
//         .map(|(index, _)| index)
//         .collect::<Vec<usize>>();

//     let filtered_data = filtered_data.ready();
//     let master_data = promised_data.ready();

//     let def_vec: Vec<Arc<StringRecord>> = vec![];
//     let default_data: Arc<Vec<Arc<StringRecord>>> = Arc::new(def_vec);

//     let sheet_data = match filtered_data {
//         Some(data) if !filter.is_empty() => data,
//         _ => master_data.unwrap_or(&default_data),
//     };

//     return table_ui.body(|body| {
//         let table_height = sheet_data.len();

//         body.rows(18.0, table_height, |mut row| {
//             let row_index = row.index();

//             let row_data = sheet_data.get(row_index).unwrap();

//             row_data
//                 .iter()
//                 .map(|text| text.to_string())
//                 .enumerate()
//                 .filter(|(index, _)| visible_columns.contains(index))
//                 .for_each(|(col_index, text)| {
//                     let filter_text: &str = text.as_ref();

//                     row.col(|ui| {
//                         let label = if filter.is_empty() {
//                             ui.label(filter_text)
//                         } else {
//                             use egui::text::LayoutJob;

//                             if filter_text.contains(&filter) {
//                                 let mut job = LayoutJob::default();

//                                 if filter_text == filter {
//                                     job.append(
//                                         filter_text,
//                                         0.0,
//                                         TextFormat {
//                                             color: Color32::YELLOW,
//                                             ..Default::default()
//                                         },
//                                     );

//                                     ui.label(job)
//                                 } else {
//                                     let text: Vec<&str> = filter_text.split(&filter).collect();

//                                     if text.len() == 1 {
//                                         job.append(
//                                             &filter,
//                                             0.0,
//                                             TextFormat {
//                                                 color: Color32::YELLOW,
//                                                 ..Default::default()
//                                             },
//                                         );
//                                         job.append(text[0], 0.0, TextFormat::default());
//                                         ui.label(job)
//                                     } else if text.len() == 2 {
//                                         job.append(text[0], 0.0, TextFormat::default());
//                                         job.append(
//                                             &filter,
//                                             0.0,
//                                             TextFormat {
//                                                 color: Color32::YELLOW,
//                                                 ..Default::default()
//                                             },
//                                         );
//                                         job.append(text[1], 0.0, TextFormat::default());

//                                         ui.label(job)
//                                     } else {
//                                         ui.label(job)
//                                     }
//                                 }
//                             } else {
//                                 ui.label(filter_text)
//                             }
//                         };

//                         if label.clicked() {
//                             ctx.input(|input| {
//                                 if input.modifiers.command {
//                                     if let Err(e) = &sender
//                                         .send(UiMessage::FilterGlobal(filter_text.to_string()))
//                                     {
//                                         eprintln!(
//                                             "Worker: Failed to send page data to UI thread: {:?}",
//                                             e
//                                         );
//                                     }
//                                 } else {
//                                     if let Err(e) = &sender.send(UiMessage::FilterSheet(
//                                         filename.to_string(),
//                                         filter_text.to_string(),
//                                         tab_id,
//                                         Some(col_index),
//                                     )) {
//                                         eprintln!(
//                                             "Worker: Failed to send page data to UI thread: {:?}",
//                                             e
//                                         );
//                                     }
//                                 }
//                             })
//                         }
//                     });
//                 });
//         });
//     });
// }
