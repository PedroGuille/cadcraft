//! Safe JNI calls into the small Android document-picker host.
#![forbid(unsafe_code)]

use jni::objects::{JClass, JObject, JString, JValue};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

static CALLBACK: OnceLock<Mutex<Option<(egui::Context, crate::recovery::Control)>>> = OnceLock::new();
static EVENTS_PENDING: AtomicBool = AtomicBool::new(true);

pub fn register(ctx: egui::Context, control: crate::recovery::Control) {
    if let Ok(mut callback) = CALLBACK.get_or_init(|| Mutex::new(None)).lock() {
        *callback = Some((ctx, control));
    }
}
pub fn wake(kind: i32) {
    EVENTS_PENDING.store(true, Ordering::Release);
    if let Some(callback) = CALLBACK.get()
        && let Ok(callback) = callback.lock()
        && let Some((ctx, control)) = &*callback
    {
        if kind == 1 {
            control.flush();
        }
        ctx.request_repaint();
    }
}
pub fn has_events() -> bool {
    EVENTS_PENDING.swap(false, Ordering::AcqRel)
}

fn call<R>(f: impl FnOnce(&mut jni::JNIEnv<'_>, &JObject<'_>) -> jni::errors::Result<R>) -> Result<R, String> {
    robius_android_env::with_activity(|env, context| {
        // The native event thread stays attached to the VM. Release each call's temporary
        // Java references so polling the document queues cannot fill the local-reference table.
        let result = env.with_local_frame(16, |env| {
            // android-activity 0.6 keeps the Application in ndk-context. Resolve our
            // live Activity through its application class loader without raw JNI pointers.
            let loader = env.call_method(context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
            let name = JObject::from(env.new_string("io.github.storytold.cadcraft.MainActivity")?);
            let class = env.call_method(loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
            let activity =
                env.call_static_method(JClass::from(class), "currentActivity", "()Lio/github/storytold/cadcraft/MainActivity;", &[])?.l()?;
            if activity.is_null() {
                return Err(jni::errors::Error::NullPtr("Android Activity"));
            }
            f(env, &activity)
        });
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result
    })
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

pub fn pick_open() -> Result<(), String> {
    call(|env, activity| env.call_method(activity, "pickDrawing", "()V", &[]).map(|_| ()))
}

pub fn background() -> Result<(), String> {
    call(|env, activity| env.call_method(activity, "background", "()V", &[]).map(|_| ()))
}

pub fn input(text: &str, prompt: &str, numeric: bool) -> Result<(), String> {
    call(|env, activity| {
        let text = JObject::from(env.new_string(text)?);
        let prompt = JObject::from(env.new_string(prompt)?);
        env.call_method(
            activity,
            "enterCommand",
            "(Ljava/lang/String;Ljava/lang/String;Z)V",
            &[JValue::Object(&text), JValue::Object(&prompt), JValue::Bool(u8::from(numeric))],
        )?;
        Ok(())
    })
}

pub fn export(path: &str, name: &str) -> Result<(), String> {
    call(|env, activity| {
        let path = JObject::from(env.new_string(path)?);
        let name = JObject::from(env.new_string(name)?);
        env.call_method(activity, "exportDrawing", "(Ljava/lang/String;Ljava/lang/String;)V", &[JValue::Object(&path), JValue::Object(&name)])?;
        Ok(())
    })
}

pub fn take(method: &str) -> Result<Option<String>, String> {
    call(|env, activity| {
        let value = env.call_method(activity, method, "()Ljava/lang/String;", &[])?.l()?;
        if value.is_null() {
            return Ok(None);
        }
        let text = JString::from(value);
        Ok(Some(env.get_string(&text)?.into()))
    })
}

fn string_call(method: &str, text: &str) -> Result<(), String> {
    call(|env, activity| {
        let text = JObject::from(env.new_string(text)?);
        env.call_method(activity, method, "(Ljava/lang/String;)V", &[JValue::Object(&text)])?;
        Ok(())
    })
}
pub fn labels(text: &str) -> Result<(), String> {
    string_call("setLabels", text)
}
pub fn name(text: &str) -> Result<(), String> {
    string_call("nameDrawing", text)
}
pub fn sync_linked(path: &str) -> Result<(), String> {
    string_call("syncLinkedDrawing", path)
}
pub fn memory_budget() -> Result<u64, String> {
    call(|env, activity| env.call_method(activity, "memoryBudget", "()J", &[])?.j()).map(|n| n.clamp(32 << 20, 256 << 20) as u64)
}
pub fn memory_state() -> Result<crate::memory::State, String> {
    let text = take("memoryState")?.ok_or("Android did not return memory information")?;
    let mut fields = text.split(',');
    let total = fields.next().and_then(|s| s.parse().ok()).ok_or("Invalid total memory")?;
    let available = fields.next().and_then(|s| s.parse().ok()).ok_or("Invalid available memory")?;
    let low = fields.next() == Some("true");
    Ok(crate::memory::State { total, available, low })
}
pub fn pick_special(project: bool) -> Result<(), String> {
    call(|env, activity| env.call_method(activity, if project { "pickProject" } else { "pickLinked" }, "()V", &[]).map(|_| ()))
}
pub fn share(path: &str, name: &str) -> Result<(), String> {
    call(|env, activity| {
        let path = JObject::from(env.new_string(path)?);
        let name = JObject::from(env.new_string(name)?);
        env.call_method(activity, "shareDrawing", "(Ljava/lang/String;Ljava/lang/String;)V", &[JValue::Object(&path), JValue::Object(&name)])?;
        Ok(())
    })
}
