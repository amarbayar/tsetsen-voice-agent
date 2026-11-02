//! Pictures: finding them by when they were taken, and putting them on
//! screen so the user can see which ones are meant.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use serde::Deserialize;
use tsetsen_core::Ctx;
use tsetsen_core::ports::{PhotoInfo, PhotoLibrary, PhotoQuery, ToolCall, ToolSpec};
use tsetsen_policy::Consequence;

use super::items::Item;
use super::{MAX_SHOWN, Shared, args, explain, periods, plural, spec};
use crate::tools::{Assessment, ToolOutput, Toolbox};
use crate::when::{Period, day_label};

const FIND_PICTURES: &str = "find_pictures";
/// Pictures are resized to this many pixels on their longer side: sharp on
/// any screen, small enough to send several.
const EXPORT_PIXELS: u32 = 1600;
/// The most photos read from the library per search.
const SEARCH_LIMIT: usize = 500;

/// Finding pictures.
pub(crate) struct PictureTools {
    shared: Arc<Shared>,
    photos: Arc<dyn PhotoLibrary>,
}

pub(crate) fn offer(shared: &Arc<Shared>) -> Option<Arc<dyn Toolbox>> {
    let photos = shared.caps.photos.clone()?;
    Some(Arc::new(PictureTools {
        shared: Arc::clone(shared),
        photos,
    }))
}

#[derive(Deserialize)]
struct FindArgs {
    #[serde(default, alias = "date", alias = "time")]
    when: Option<String>,
}

/// At most `max` of `photos` (oldest first), favorites first, the rest
/// spread evenly over the period.
fn pick(mut photos: Vec<PhotoInfo>, max: usize) -> Vec<PhotoInfo> {
    photos.sort_by_key(|p| p.taken);
    if photos.len() <= max {
        return photos;
    }
    let (favorites, others): (Vec<_>, Vec<_>) = photos.into_iter().partition(|p| p.favorite);
    let mut chosen: Vec<PhotoInfo> = favorites.into_iter().take(max).collect();
    let room = max - chosen.len();
    if room > 0 {
        // Evenly spaced: the i-th of `room` picks sits at i * len / room.
        chosen.extend((0..room).map(|i| others[i * others.len() / room].clone()));
    }
    chosen.sort_by_key(|p| p.taken);
    chosen
}

impl PictureTools {
    async fn find(&self, when: Option<&str>, cx: &Ctx) -> ToolOutput {
        let now = (self.shared.clock)();
        let alternatives = match periods(when, &now, "recently") {
            Ok(found) => found,
            Err(problem) => return ToolOutput::failed(problem),
        };
        let mut matches: Vec<(Period, Vec<PhotoInfo>)> = Vec::new();
        for (period, from, to) in alternatives {
            let query = PhotoQuery {
                from,
                to,
                limit: SEARCH_LIMIT,
            };
            match self.photos.search(&query, cx).await {
                Ok(found) => matches.push((period, found)),
                Err(error) => return ToolOutput::failed(explain(&error, "The photo library")),
            }
        }
        let with_pictures: Vec<&(Period, Vec<PhotoInfo>)> =
            matches.iter().filter(|(_, p)| !p.is_empty()).collect();
        match with_pictures[..] {
            [] => self.nothing_found(&matches, &now, cx).await,
            [(period, found)] => self.show(period, found.clone(), now.time_zone(), cx).await,
            _ => {
                let options: Vec<String> = with_pictures
                    .iter()
                    .map(|(p, found)| {
                        format!(
                            "{} has {}",
                            p.label,
                            plural(found.len(), "picture", "pictures")
                        )
                    })
                    .collect();
                ToolOutput::ok(format!(
                    "The user could mean more than one day: {}. Ask which one they mean, in a \
                     few words, then call {FIND_PICTURES} again with that day.",
                    options.join("; ")
                ))
            }
        }
    }

    /// Nothing in the period: say which recent days do have pictures, so
    /// the user is offered something instead of a dead end.
    async fn nothing_found(
        &self,
        searched: &[(Period, Vec<PhotoInfo>)],
        now: &Zoned,
        cx: &Ctx,
    ) -> ToolOutput {
        let labels: Vec<&str> = searched.iter().map(|(p, _)| p.label.as_str()).collect();
        let year_ago = now.checked_sub(1.year()).unwrap_or_else(|_| now.clone());
        let query = PhotoQuery {
            from: year_ago.timestamp().into(),
            to: now.timestamp().into(),
            limit: SEARCH_LIMIT,
        };
        let recent = match self.photos.search(&query, cx).await {
            Ok(found) => found,
            Err(error) => return ToolOutput::failed(explain(&error, "The photo library")),
        };
        let mut days: Vec<(jiff::civil::Date, usize)> = Vec::new();
        for photo in &recent {
            let Ok(taken) = Timestamp::try_from(photo.taken) else {
                continue;
            };
            let date = taken.to_zoned(now.time_zone().clone()).date();
            match days.iter_mut().find(|(d, _)| *d == date) {
                Some((_, n)) => *n += 1,
                None => days.push((date, 1)),
            }
        }
        days.sort_by_key(|d| std::cmp::Reverse(d.0));
        let nothing = format!("There are no pictures from {}.", labels.join(" or "));
        if days.is_empty() {
            return ToolOutput::ok(format!(
                "{nothing} There are no pictures from the past year at all. Tell the user."
            ));
        }
        let recent: Vec<String> = days
            .iter()
            .take(3)
            .map(|(d, n)| {
                format!(
                    "{}: {}",
                    day_label(*d, now.date()),
                    plural(*n, "picture", "pictures")
                )
            })
            .collect();
        ToolOutput::ok(format!(
            "{nothing} The most recent days with pictures are {}. Nothing is shown yet. Tell the \
             user, and ask if one of those is what they mean; if so, call {FIND_PICTURES} with \
             that day to show them.",
            recent.join("; ")
        ))
    }

    async fn show(
        &self,
        period: &Period,
        found: Vec<PhotoInfo>,
        tz: &TimeZone,
        cx: &Ctx,
    ) -> ToolOutput {
        let total = found.len();
        let chosen = pick(found, MAX_SHOWN);
        let ids: Vec<String> = chosen.iter().map(|p| p.id.clone()).collect();
        let exported = match self.photos.export(&ids, EXPORT_PIXELS, cx).await {
            Ok(exported) => exported,
            Err(error) => return ToolOutput::failed(explain(&error, "The photo library")),
        };
        let paths: Vec<_> = exported.iter().map(|e| e.path.clone()).collect();
        let shown = super::show::present(&self.shared, &paths, cx).await;

        let mut out = format!(
            "Found {} from {}",
            plural(total, "picture", "pictures"),
            period.label
        );
        if total > exported.len() {
            let _ = write!(out, "; picked {} of them", exported.len());
        }
        out.push_str(if shown {
            ". They are on the screen now, numbered in this order:\n"
        } else {
            ". They could not be shown on screen. In order:\n"
        });
        for (number, file) in exported.into_iter().enumerate() {
            let Some(info) = chosen.iter().find(|c| c.id == file.id).cloned() else {
                continue;
            };
            let time = Timestamp::try_from(info.taken)
                .map(|t| t.to_zoned(tz.clone()).strftime("%-I:%M %p").to_string())
                .unwrap_or_default();
            let favorite = if info.favorite { ", a favorite" } else { "" };
            let handle = self.shared.items.add(Item::Picture { info, file });
            let _ = writeln!(
                out,
                "picture {} is {handle}, taken at {time}{favorite}",
                number + 1
            );
        }
        out.push_str(
            "Tell the user how many you found and that they are on the screen, then ask which \
             ones they want, or whether all of them.",
        );
        ToolOutput::ok(out)
    }
}

#[async_trait]
impl Toolbox for PictureTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![spec(
            FIND_PICTURES,
            "Find the user's pictures from a time and show them on the screen, numbered. Pass \
             the time in the user's own words; the dates are worked out for you. Returns ids \
             like p1 that other tools take.",
            r#"{"type":"object","properties":{"when":{"type":"string","description":"The user's words for the time, e.g. last Saturday, our trip in August, yesterday"}},"required":["when"]}"#,
        )]
    }

    async fn assess(&self, call: &ToolCall, _cx: &Ctx) -> Result<Assessment, String> {
        let FindArgs { when } = args(call)?;
        Ok(Assessment {
            summary: match when {
                Some(when) => format!("find pictures from {}", when.trim()),
                None => "find recent pictures".into(),
            },
            consequence: Consequence::Observe,
            progress: Some("Looking through your pictures".into()),
        })
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        match args::<FindArgs>(call) {
            Ok(FindArgs { when }) => self.find(when.as_deref(), cx).await,
            Err(problem) => ToolOutput::failed(problem),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    fn photo(id: &str, secs: u64, favorite: bool) -> PhotoInfo {
        PhotoInfo {
            id: id.into(),
            taken: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            location: None,
            favorite,
        }
    }

    #[test]
    fn picks_favorites_then_spreads_the_rest() {
        let photos: Vec<PhotoInfo> = (0..30)
            .map(|i| photo(&format!("{i}"), i, i == 29))
            .collect();
        let picked = pick(photos, 4);
        let ids: Vec<&str> = picked.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["0", "9", "19", "29"],
            "oldest first, the favorite kept"
        );
        assert_eq!(
            pick(vec![photo("a", 2, false), photo("b", 1, false)], 4)[0].id,
            "b"
        );
    }
}
