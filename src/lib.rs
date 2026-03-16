use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use mlua::serde::LuaSerdeExt;
use mlua::{Lua, Table, UserData, UserDataMethods, Value};

fn now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Clone, Debug)]
struct DanmakuMessage {
    nickname: String,
    text: String,
    timeline: String,
    ts: f64,
    raw: serde_json::Value,
}

#[derive(Clone)]
struct RoomInfo {
    room_id: i64,
    live_status: i64,
    title: String,
    online: i64,
    live_time: String,
    raw: serde_json::Value,
}

fn parse_timeline_ts(timeline: &str) -> f64 {
    let s = timeline.trim();
    if s.is_empty() {
        return 0.0;
    }
    let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") else {
        return 0.0;
    };
    // Python datetime.timestamp() on naive datetime assumes local time.
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}

#[derive(Clone)]
struct BilibiliLivePoller {
    room_id: i64,
    user_agent: String,
    base_url: String,
    dedupe_size: usize,
    last_seen_ts: f64,
    dedupe: VecDeque<(String, String, String)>,
    dedupe_set: HashSet<(String, String, String)>,
    agent: ureq::Agent,
}

impl BilibiliLivePoller {
    fn new(room_id: i64, user_agent: Option<String>, base_url: Option<String>, dedupe_size: usize) -> Self {
        let ua = user_agent.unwrap_or_default().trim().to_string();
        let user_agent = if ua.is_empty() {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
                .to_string()
        } else {
            ua
        };
        let base_url = base_url
            .unwrap_or_else(|| "https://api.live.bilibili.com/ajax/msg".to_string())
            .trim()
            .trim_end_matches('/')
            .to_string();
        Self {
            room_id,
            user_agent,
            base_url,
            dedupe_size: dedupe_size.max(1),
            last_seen_ts: now_s(),
            dedupe: VecDeque::new(),
            dedupe_set: HashSet::new(),
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    fn seen(&self, key: &(String, String, String)) -> bool {
        self.dedupe_set.contains(key)
    }

    fn mark_seen(&mut self, key: (String, String, String)) {
        if self.dedupe_set.contains(&key) {
            return;
        }
        self.dedupe.push_back(key.clone());
        self.dedupe_set.insert(key);
        while self.dedupe.len() > self.dedupe_size {
            if let Some(old) = self.dedupe.pop_front() {
                self.dedupe_set.remove(&old);
            }
        }
    }

    fn fetch(&self, timeout_s: f64) -> Result<Vec<DanmakuMessage>> {
        let url = format!("{}?roomid={}", self.base_url, self.room_id);
        let req = self
            .agent
            .get(&url)
            .set("User-Agent", &self.user_agent)
            .set("Accept", "application/json, text/plain, */*")
            .set("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .set("Referer", &format!("https://live.bilibili.com/{}", self.room_id))
            .timeout(Duration::from_secs_f64(timeout_s.max(1.0)));

        let resp = req.call().map_err(|e| anyhow!("Failed to fetch bilibili ajax/msg: {e}"))?;
        let payload = resp.into_string().unwrap_or_default();
        let data: serde_json::Value = serde_json::from_str(&payload).map_err(|e| {
            anyhow!(
                "Invalid JSON from bilibili ajax/msg: {e}: {}",
                payload.chars().take(200).collect::<String>()
            )
        })?;

        if let Some(code) = data.get("code").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                return Err(anyhow!("bilibili ajax/msg code={code} message={msg}"));
            }
        }

        let room = data
            .get("data")
            .and_then(|v| v.get("room"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out: Vec<DanmakuMessage> = Vec::new();
        for item in room {
            let Some(obj) = item.as_object() else { continue };
            let nickname = obj.get("nickname").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let timeline = obj.get("timeline").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if nickname.is_empty() || text.is_empty() {
                continue;
            }
            let ts = parse_timeline_ts(&timeline);
            out.push(DanmakuMessage {
                nickname,
                text,
                timeline,
                ts,
                raw: item,
            });
        }
        Ok(out)
    }

    fn poll_new(&mut self, timeout_s: f64) -> Result<Vec<DanmakuMessage>> {
        let messages = self.fetch(timeout_s)?;
        let mut new_msgs: Vec<DanmakuMessage> = Vec::new();
        let mut max_ts = self.last_seen_ts;

        for m in messages {
            let key = (m.timeline.clone(), m.nickname.clone(), m.text.clone());
            if self.seen(&key) {
                continue;
            }
            if m.ts != 0.0 && m.ts < self.last_seen_ts {
                continue;
            }
            self.mark_seen(key);
            if m.ts != 0.0 && m.ts > max_ts {
                max_ts = m.ts;
            }
            new_msgs.push(m);
        }

        self.last_seen_ts = max_ts;
        Ok(new_msgs)
    }
}

fn get_room_info(room_id: i64, user_agent: Option<String>, timeout_s: f64) -> Result<RoomInfo> {
    let ua = user_agent.unwrap_or_default().trim().to_string();
    let user_agent = if ua.is_empty() {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
            .to_string()
    } else {
        ua
    };

    let url = format!(
        "https://api.live.bilibili.com/room/v1/Room/get_info?room_id={}",
        room_id
    );
    let agent = ureq::AgentBuilder::new().build();
    let req = agent
        .get(&url)
        .set("User-Agent", &user_agent)
        .timeout(Duration::from_secs_f64(timeout_s.max(1.0)));

    let resp = req.call().map_err(|e| anyhow!("Failed to fetch bilibili room/get_info: {e}"))?;
    let payload = resp.into_string().unwrap_or_default();
    let obj: serde_json::Value = serde_json::from_str(&payload).map_err(|e| {
        anyhow!(
            "Invalid JSON from bilibili room/get_info: {e}: {}",
            payload.chars().take(200).collect::<String>()
        )
    })?;

    let code = obj.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(anyhow!(
            "Unexpected bilibili room/get_info code={code}: {}",
            payload.chars().take(200).collect::<String>()
        ));
    }

    let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let Some(d) = data.as_object() else {
        return Err(anyhow!("Invalid bilibili room/get_info payload (missing data)."));
    };

    Ok(RoomInfo {
        room_id: d.get("room_id").and_then(|v| v.as_i64()).unwrap_or(room_id),
        live_status: d.get("live_status").and_then(|v| v.as_i64()).unwrap_or(0),
        title: d.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        online: d.get("online").and_then(|v| v.as_i64()).unwrap_or(0),
        live_time: d.get("live_time").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        raw: data,
    })
}

struct QueueInner<T> {
    buf: VecDeque<T>,
    cap: usize,
    stopped_reason: Option<String>,
}

struct BoundedQueue<T> {
    inner: Mutex<QueueInner<T>>,
    cv: Condvar,
}

impl<T> BoundedQueue<T> {
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                buf: VecDeque::new(),
                cap: cap.max(1),
                stopped_reason: None,
            }),
            cv: Condvar::new(),
        }
    }

    fn push_drop_oldest(&self, item: T) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.buf.len() >= g.cap {
            g.buf.pop_front();
        }
        g.buf.push_back(item);
        self.cv.notify_one();
    }

    fn stop(&self, reason: Option<&str>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.stopped_reason.is_none() {
            g.stopped_reason = reason.map(|s| s.to_string()).or(Some("stopped".to_string()));
        }
        self.cv.notify_all();
    }

    fn stopped_reason(&self) -> Option<String> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.stopped_reason.clone()
    }

    fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.buf.is_empty() && g.stopped_reason.is_none() {
            let (guard, _waited) = match self.cv.wait_timeout(g, timeout) {
                Ok(res) => res,
                Err(poison) => poison.into_inner(),
            };
            g = guard;
        }
        g.buf.pop_front()
    }
}

pub struct DanmakuStream {
    queue: Arc<BoundedQueue<DanmakuMessage>>,
    stop_flag: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl DanmakuStream {
    fn stop_inner(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.queue.stop(Some("stopped"));
        if let Ok(mut g) = self.join.lock() {
            if let Some(h) = g.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for DanmakuStream {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl UserData for DanmakuStream {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("next", |lua, this, timeout_s: Option<f64>| {
            let timeout = Duration::from_secs_f64(timeout_s.unwrap_or(0.2).max(0.0));
            let Some(msg) = this.queue.pop_timeout(timeout) else {
                return Ok(Value::Nil);
            };

            let t = lua.create_table()?;
            t.set("nickname", msg.nickname)?;
            t.set("text", msg.text)?;
            t.set("timeline", msg.timeline)?;
            t.set("ts", msg.ts)?;
            let raw_val = lua.to_value(&msg.raw)?;
            t.set("raw", raw_val)?;
            Ok(Value::Table(t))
        });

        methods.add_method("stop", |_lua, this, ()| {
            this.stop_inner();
            Ok(())
        });

        methods.add_method("stopped_reason", |_lua, this, ()| Ok(this.queue.stopped_reason()));
    }
}

fn sleep_with_stop(duration: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while start.elapsed() < duration {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn register_module(lua: &Lua, mori: &Table) -> Result<()> {
    let live_stream = lua.create_table()?;

    let get_room_info_fn = lua.create_function(|lua, opts: Table| {
        let room_id: i64 = opts.get("room_id")?;
        let user_agent: Option<String> = opts.get("user_agent").ok();
        let timeout_s: f64 = opts.get("timeout_s").unwrap_or(10.0);

        let info = get_room_info(room_id, user_agent, timeout_s).map_err(mlua::Error::external)?;

        let t = lua.create_table()?;
        t.set("room_id", info.room_id)?;
        t.set("live_status", info.live_status)?;
        t.set("title", info.title)?;
        t.set("online", info.online)?;
        t.set("live_time", info.live_time)?;
        t.set("raw", lua.to_value(&info.raw)?)?;
        Ok(t)
    })?;
    live_stream.set("get_room_info", get_room_info_fn)?;

    let start_stream_fn = lua.create_function(|_lua, opts: Table| {
        let room_id: i64 = opts.get("room_id")?;
        if room_id <= 0 {
            return Err(mlua::Error::external("room_id must be > 0"));
        }
        let interval_s: f64 = opts.get("interval_s").unwrap_or(2.0);
        let catchup_n: i64 = opts.get("catchup_n").unwrap_or(0);
        let dedupe_size: i64 = opts.get("dedupe_size").unwrap_or(512);
        let user_agent: Option<String> = opts.get("user_agent").ok();
        let exit_when_offline: bool = opts.get("exit_when_offline").unwrap_or(false);
        let live_check_interval_s: f64 = opts.get("live_check_interval_s").unwrap_or(15.0);
        let queue_cap: i64 = opts.get("queue_cap").unwrap_or(512);

        let queue = Arc::new(BoundedQueue::new(queue_cap.max(1) as usize));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let queue_cloned = Arc::clone(&queue);
        let stop_cloned = Arc::clone(&stop_flag);

        let join = std::thread::spawn(move || {
            let mut poller = BilibiliLivePoller::new(room_id, user_agent, None, dedupe_size.max(1) as usize);

            let mut catchup = catchup_n;
            if catchup < 0 {
                catchup = 0;
            }
            if catchup > 10 {
                catchup = 10;
            }

            if catchup > 0 {
                match poller.fetch(10.0) {
                    Ok(mut current) => {
                        current.sort_by(|a, b| {
                            a.ts.partial_cmp(&b.ts)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.timeline.cmp(&b.timeline))
                                .then_with(|| a.nickname.cmp(&b.nickname))
                                .then_with(|| a.text.cmp(&b.text))
                        });
                        let take_n = catchup as usize;
                        if current.len() > take_n {
                            current = current[current.len() - take_n..].to_vec();
                        }
                        for msg in current {
                            let key = (msg.timeline.clone(), msg.nickname.clone(), msg.text.clone());
                            poller.mark_seen(key);
                            queue_cloned.push_drop_oldest(msg);
                        }
                    }
                    Err(e) => {
                        eprintln!("bili> fetch error: {e}");
                    }
                }
            }

            let mut next_live_check = Instant::now() + Duration::from_secs_f64(live_check_interval_s.max(1.0));

            loop {
                if stop_cloned.load(Ordering::Relaxed) {
                    break;
                }

                if exit_when_offline && Instant::now() >= next_live_check {
                    match get_room_info(room_id, None, 10.0) {
                        Ok(info) => {
                            if info.live_status != 1 {
                                queue_cloned.stop(Some("offline"));
                                stop_cloned.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                        Err(e) => eprintln!("bili> live check error: {e}"),
                    }
                    next_live_check = Instant::now() + Duration::from_secs_f64(live_check_interval_s.max(1.0));
                }

                match poller.poll_new(10.0) {
                    Ok(msgs) => {
                        for msg in msgs {
                            queue_cloned.push_drop_oldest(msg);
                        }
                    }
                    Err(e) => {
                        eprintln!("bili> poll error: {e}");
                    }
                }

                sleep_with_stop(Duration::from_secs_f64(interval_s.max(0.05)), &stop_cloned);
            }

            queue_cloned.stop(Some("stopped"));
        });

        Ok(DanmakuStream {
            queue,
            stop_flag,
            join: Mutex::new(Some(join)),
        })
    })?;
    live_stream.set("start_danmaku_stream", start_stream_fn)?;

    mori.set("live_stream", live_stream)?;
    Ok(())
}

