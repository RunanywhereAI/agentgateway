//! What reading one field of a crafted request body costs in memory.
//!
//! A policy reads `json(request.body).model` before the caller is authenticated, and the optimizer
//! rewrites that to `jsonField(request.body, "model")`. A body of about 8 MiB whose `model` is an
//! array of empty objects used to be built into a full value tree and then into a CEL list, hundreds
//! of MiB for one request. The field must now be refused without that tree, and the memory the read
//! takes must stay under twice the body.
//!
//! This file is its own test binary so that its allocator sees only this test; keep it to a single
//! `#[test]`, because a second one running at the same time would count into the same peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use cel::context::{DefaultVariableResolver, SingleVarResolver};
use cel::objects::BytesValue;
use cel::{Context, Program, Value};
use serde_json::json;

/// The system allocator, counting the bytes that are live and the most that were live at once.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
	let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
	PEAK.fetch_max(live, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		let ptr = unsafe { System.alloc(layout) };
		if !ptr.is_null() {
			grow(layout.size());
		}
		ptr
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		unsafe { System.dealloc(ptr, layout) };
		LIVE.fetch_sub(layout.size(), Relaxed);
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		let moved = unsafe { System.realloc(ptr, layout, new_size) };
		if !moved.is_null() {
			// The old and the new block count together until the old one is released, which is what
			// a moving realloc holds.
			grow(new_size);
			LIVE.fetch_sub(layout.size(), Relaxed);
		}
		moved
	}
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const BODY_BYTES: usize = 8 * 1024 * 1024;

/// `{"model":[{},{},...],"max_tokens":16}`, a little over `BODY_BYTES`.
fn crafted_body() -> String {
	let mut body = String::with_capacity(BODY_BYTES + 64);
	body.push_str(r#"{"model":["#);
	while body.len() < BODY_BYTES {
		body.push_str("{},");
	}
	body.push_str(r#"{}],"max_tokens":16}"#);
	body
}

#[test]
fn a_crafted_eight_mib_model_is_refused_and_costs_less_than_twice_the_body() {
	let body = crafted_body();
	let model =
		Program::compile_with_optimizer("json(body).model", agent_celx::DefaultOptimizer).unwrap();
	let ceiling =
		Program::compile_with_optimizer("json(body).max_tokens", agent_celx::DefaultOptimizer).unwrap();
	let mut context = Context::default();
	agent_celx::insert_all(&mut context);
	// The body is lent to the expression, so the read itself is the only thing that allocates.
	let resolver = SingleVarResolver::new(
		&DefaultVariableResolver,
		"body",
		Value::Bytes(BytesValue::Borrowed(body.as_bytes())),
	);

	PEAK.store(LIVE.load(Relaxed), Relaxed);
	let read_model = Value::resolve(model.expression(), &context, &resolver);
	let model_was_refused = read_model.is_err();
	drop(read_model);
	let read_ceiling = Value::resolve(ceiling.expression(), &context, &resolver)
		.expect("a key after the refused value is still found");
	let peak = PEAK.load(Relaxed);

	assert_eq!(read_ceiling.json().unwrap(), json!(16));
	assert!(
		peak < 2 * body.len(),
		"reading two fields of a {} byte body took a peak of {peak} bytes",
		body.len()
	);
	assert!(
		model_was_refused,
		"a `model` that is an array of {} objects was returned",
		body.matches("{}").count()
	);
}
