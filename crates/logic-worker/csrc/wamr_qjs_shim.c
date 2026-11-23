#define _POSIX_C_SOURCE 200809L
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <time.h>

#include <pthread.h>
#include "wasm_export.h"
#include "quickjs.h"

/* ---------------- Monotonic clock helper ---------------- */
static uint64_t monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

/* ---------------- WAMR Runtime Shim (A1) ---------------- */
typedef struct {
    uint8_t *code_buf;
    uint32_t code_len;
    wasm_module_t module;
    wasm_module_inst_t inst;
    wasm_exec_env_t exec_env;
    wasm_function_inst_t eval_fn;
    uint32_t req_app_ptr;
    uint32_t res_app_ptr;
    uint32_t buf_cap;
} PlcWamrSlot;

static pthread_once_t g_wamr_once = PTHREAD_ONCE_INIT;
static int g_wamr_init_ok = 0;

static void wamr_init_once_fn(void) {
    RuntimeInitArgs init_args;
    memset(&init_args, 0, sizeof(init_args));
    init_args.mem_alloc_type = Alloc_With_System_Allocator;
    g_wamr_init_ok = wasm_runtime_full_init(&init_args) ? 1 : 0;
}

int plc_wamr_global_init(void) {
    pthread_once(&g_wamr_once, wamr_init_once_fn);
    return g_wamr_init_ok ? 0 : -1;
}

PlcWamrSlot *plc_wamr_load_and_warm(
    const uint8_t *bytes,
    uint32_t len,
    uint32_t stack_size,
    uint32_t heap_size,
    uint32_t io_buf_cap,
    char *err_buf,
    uint32_t err_cap
) {
    if (plc_wamr_global_init() != 0) {
        snprintf(err_buf, err_cap, "wasm_runtime_full_init failed");
        return NULL;
    }
    if (!bytes || len == 0) {
        snprintf(err_buf, err_cap, "empty module bytes");
        return NULL;
    }
    PlcWamrSlot *slot = (PlcWamrSlot *)calloc(1, sizeof(PlcWamrSlot));
    if (!slot) {
        snprintf(err_buf, err_cap, "oom allocating PlcWamrSlot");
        return NULL;
    }
    slot->code_buf = (uint8_t *)malloc(len);
    if (!slot->code_buf) {
        free(slot);
        snprintf(err_buf, err_cap, "oom copying module bytes");
        return NULL;
    }
    memcpy(slot->code_buf, bytes, len);
    slot->code_len = len;

    slot->module = wasm_runtime_load(slot->code_buf, slot->code_len, err_buf, err_cap);
    if (!slot->module) {
        free(slot->code_buf);
        free(slot);
        return NULL;
    }

    slot->inst = wasm_runtime_instantiate(slot->module, stack_size, heap_size, err_buf, err_cap);
    if (!slot->inst && heap_size > 0) {
        slot->inst = wasm_runtime_instantiate(slot->module, stack_size, 0, err_buf, err_cap);
    }
    if (!slot->inst) {
        wasm_runtime_unload(slot->module);
        free(slot->code_buf);
        free(slot);
        return NULL;
    }

    slot->exec_env = wasm_runtime_create_exec_env(slot->inst, stack_size);
    if (!slot->exec_env) {
        snprintf(err_buf, err_cap, "wasm_runtime_create_exec_env failed");
        wasm_runtime_deinstantiate(slot->inst);
        wasm_runtime_unload(slot->module);
        free(slot->code_buf);
        free(slot);
        return NULL;
    }

    slot->eval_fn = wasm_runtime_lookup_function(slot->inst, "eval_scan");
    if (!slot->eval_fn ||
        wasm_func_get_param_count(slot->eval_fn, slot->inst) != 4 ||
        wasm_func_get_result_count(slot->eval_fn, slot->inst) != 1) {
        snprintf(err_buf, err_cap, "missing or invalid eval_scan(4->1) signature");
        wasm_runtime_destroy_exec_env(slot->exec_env);
        wasm_runtime_deinstantiate(slot->inst);
        wasm_runtime_unload(slot->module);
        free(slot->code_buf);
        free(slot);
        return NULL;
    }

    int have_exports = 0;
    wasm_function_inst_t get_req_fn = wasm_runtime_lookup_function(slot->inst, "get_req_buf");
    wasm_function_inst_t get_res_fn = wasm_runtime_lookup_function(slot->inst, "get_res_buf");
    if (get_req_fn && get_res_fn &&
        wasm_func_get_param_count(get_req_fn, slot->inst) == 0 &&
        wasm_func_get_result_count(get_req_fn, slot->inst) == 1 &&
        wasm_func_get_param_count(get_res_fn, slot->inst) == 0 &&
        wasm_func_get_result_count(get_res_fn, slot->inst) == 1) {
        uint32_t a1[1] = {0};
        uint32_t a2[1] = {0};
        if (wasm_runtime_call_wasm(slot->exec_env, get_req_fn, 0, a1) &&
            wasm_runtime_call_wasm(slot->exec_env, get_res_fn, 0, a2) &&
            wasm_runtime_validate_app_addr(slot->inst, a1[0], io_buf_cap) &&
            wasm_runtime_validate_app_addr(slot->inst, a2[0], io_buf_cap)) {
            slot->req_app_ptr = a1[0];
            slot->res_app_ptr = a2[0];
            have_exports = 1;
        } else {
            wasm_runtime_clear_exception(slot->inst);
        }
    }
    if (!have_exports) {
        void *tmp_req = NULL;
        void *tmp_res = NULL;
        uint64_t req_off = wasm_runtime_module_malloc(slot->inst, io_buf_cap, &tmp_req);
        uint64_t res_off = wasm_runtime_module_malloc(slot->inst, io_buf_cap, &tmp_res);
        if (req_off == 0 || res_off == 0 || !tmp_req || !tmp_res) {
            snprintf(err_buf, err_cap, "failed to preallocate Wasm I/O buffers");
            wasm_runtime_destroy_exec_env(slot->exec_env);
            wasm_runtime_deinstantiate(slot->inst);
            wasm_runtime_unload(slot->module);
            free(slot->code_buf);
            free(slot);
            return NULL;
        }
        slot->req_app_ptr = (uint32_t)req_off;
        slot->res_app_ptr = (uint32_t)res_off;
    }
    slot->buf_cap = io_buf_cap;
    return slot;
}

int32_t plc_wamr_eval(
    PlcWamrSlot *slot,
    const uint8_t *req_bytes,
    uint32_t req_len,
    uint8_t *res_out,
    uint32_t res_cap,
    char *err_buf,
    uint32_t err_cap
) {
    if (!slot || !slot->eval_fn) {
        snprintf(err_buf, err_cap, "slot not ready");
        return -1;
    }
    if (req_len > slot->buf_cap || res_cap > slot->buf_cap) {
        snprintf(err_buf, err_cap, "I/O buffer capacity exceeded");
        return -2;
    }
    // Always re-validate & re-translate app pointers against live linear memory (guards memory.grow relocation)
    if (!wasm_runtime_validate_app_addr(slot->inst, slot->req_app_ptr, req_len)) {
        wasm_runtime_clear_exception(slot->inst);
        snprintf(err_buf, err_cap, "invalid req app pointer");
        return -2;
    }
    void *req_nat = wasm_runtime_addr_app_to_native(slot->inst, slot->req_app_ptr);
    if (!req_nat) {
        snprintf(err_buf, err_cap, "null req native pointer");
        return -2;
    }
    memcpy(req_nat, req_bytes, req_len);
    uint32_t argv[4];
    argv[0] = slot->req_app_ptr;
    argv[1] = req_len;
    argv[2] = slot->res_app_ptr;
    argv[3] = res_cap;

    if (!wasm_runtime_call_wasm(slot->exec_env, slot->eval_fn, 4, argv)) {
        const char *exc = wasm_runtime_get_exception(slot->inst);
        snprintf(err_buf, err_cap, "wasm trap: %s", exc ? exc : "unknown");
        wasm_runtime_clear_exception(slot->inst);
        return -3;
    }
    int32_t written = (int32_t)argv[0];
    if (written <= 0 || (uint32_t)written > res_cap) {
        snprintf(err_buf, err_cap, "invalid eval_scan return len %d", written);
        return -4;
    }
    if (!wasm_runtime_validate_app_addr(slot->inst, slot->res_app_ptr, (uint32_t)written)) {
        wasm_runtime_clear_exception(slot->inst);
        snprintf(err_buf, err_cap, "invalid res app pointer after eval");
        return -4;
    }
    void *res_nat = wasm_runtime_addr_app_to_native(slot->inst, slot->res_app_ptr);
    if (!res_nat) {
        snprintf(err_buf, err_cap, "null res native pointer after eval");
        return -4;
    }
    memcpy(res_out, res_nat, (size_t)written);
    return written;
}

void plc_wamr_terminate(PlcWamrSlot *slot) {
    if (slot && slot->inst) {
        wasm_runtime_terminate(slot->inst);
    }
}

void plc_wamr_free(PlcWamrSlot *slot) {
    if (!slot) return;
    if (slot->exec_env) wasm_runtime_destroy_exec_env(slot->exec_env);
    if (slot->inst) wasm_runtime_deinstantiate(slot->inst);
    if (slot->module) wasm_runtime_unload(slot->module);
    if (slot->code_buf) free(slot->code_buf);
    free(slot);
}

/* ---------------- QuickJS-NG Runtime Shim (A2) ---------------- */
typedef struct {
    JSRuntime *rt;
    JSContext *ctx;
    JSValue eval_fn;
    uint64_t deadline_ns;
    int abort_flag;
} PlcQjsSlot;

static int plc_qjs_interrupt_handler(JSContext *ctx, void *opaque) {
    (void)ctx;
    PlcQjsSlot *slot = (PlcQjsSlot *)opaque;
    if (!slot) return 0;
    if (slot->abort_flag) return 1;
    if (slot->deadline_ns > 0 && monotonic_ns() >= slot->deadline_ns) {
        return 1;
    }
    return 0;
}

static void strip_unsafe_js_globals(JSContext *ctx) {
    JSValue global = JS_GetGlobalObject(ctx);
    const char *forbidden[] = {"eval", "Function", "setTimeout", "setInterval", "clearTimeout", "clearInterval"};
    for (size_t i = 0; i < sizeof(forbidden) / sizeof(forbidden[0]); i++) {
        JSAtom atom = JS_NewAtom(ctx, forbidden[i]);
        JS_DeleteProperty(ctx, global, atom, 0);
        JS_FreeAtom(ctx, atom);
    }
    JS_FreeValue(ctx, global);
}

PlcQjsSlot *plc_qjs_load_and_warm(
    const char *js_src,
    size_t js_len,
    size_t mem_limit_bytes,
    size_t stack_limit_bytes,
    char *err_buf,
    uint32_t err_cap
) {
    PlcQjsSlot *slot = (PlcQjsSlot *)calloc(1, sizeof(PlcQjsSlot));
    if (!slot) {
        snprintf(err_buf, err_cap, "oom allocating PlcQjsSlot");
        return NULL;
    }
    slot->rt = JS_NewRuntime();
    if (!slot->rt) {
        free(slot);
        snprintf(err_buf, err_cap, "JS_NewRuntime failed");
        return NULL;
    }
    JS_SetMemoryLimit(slot->rt, mem_limit_bytes);
    JS_SetMaxStackSize(slot->rt, stack_limit_bytes);
    JS_SetInterruptHandler(slot->rt, plc_qjs_interrupt_handler, slot);

    slot->ctx = JS_NewContext(slot->rt);
    if (!slot->ctx) {
        JS_FreeRuntime(slot->rt);
        free(slot);
        snprintf(err_buf, err_cap, "JS_NewContext failed");
        return NULL;
    }

    // Strip dynamic code evaluation BEFORE executing js_src
    strip_unsafe_js_globals(slot->ctx);

    slot->deadline_ns = monotonic_ns() + 500000000ULL;
    JSValue res = JS_Eval(slot->ctx, js_src, js_len, "<plc-module>", JS_EVAL_TYPE_GLOBAL);
    slot->deadline_ns = 0;
    if (JS_IsException(res)) {
        JSValue exc = JS_GetException(slot->ctx);
        const char *msg = JS_ToCString(slot->ctx, exc);
        snprintf(err_buf, err_cap, "JS eval error: %s", msg ? msg : "unknown");
        if (msg) JS_FreeCString(slot->ctx, msg);
        JS_FreeValue(slot->ctx, exc);
        JS_FreeValue(slot->ctx, res);
        JS_FreeContext(slot->ctx);
        JS_FreeRuntime(slot->rt);
        free(slot);
        return NULL;
    }
    JS_FreeValue(slot->ctx, res);

    JSValue global = JS_GetGlobalObject(slot->ctx);
    slot->eval_fn = JS_GetPropertyStr(slot->ctx, global, "evalScan");
    JS_FreeValue(slot->ctx, global);
    if (!JS_IsFunction(slot->ctx, slot->eval_fn)) {
        snprintf(err_buf, err_cap, "missing global evalScan(Uint8Array) function");
        JS_FreeValue(slot->ctx, slot->eval_fn);
        JS_FreeContext(slot->ctx);
        JS_FreeRuntime(slot->rt);
        free(slot);
        return NULL;
    }
    return slot;
}

int32_t plc_qjs_eval(
    PlcQjsSlot *slot,
    const uint8_t *req_bytes,
    uint32_t req_len,
    uint8_t *res_out,
    uint32_t res_cap,
    uint64_t budget_ns,
    char *err_buf,
    uint32_t err_cap
) {
    if (!slot || !slot->ctx) {
        snprintf(err_buf, err_cap, "QuickJS slot not ready");
        return -1;
    }
    JSValue arg = JS_NewUint8ArrayCopy(slot->ctx, req_bytes, req_len);
    if (JS_IsException(arg)) {
        JSValue exc = JS_GetException(slot->ctx);
        JS_FreeValue(slot->ctx, exc);
        snprintf(err_buf, err_cap, "failed to allocate input Uint8Array");
        return -2;
    }
    slot->abort_flag = 0;
    slot->deadline_ns = budget_ns > 0 ? (monotonic_ns() + budget_ns) : 0;

    JSValue argv[1] = { arg };
    JSValue ret = JS_Call(slot->ctx, slot->eval_fn, JS_UNDEFINED, 1, argv);
    slot->deadline_ns = 0;
    JS_FreeValue(slot->ctx, arg);

    if (JS_IsException(ret)) {
        JSValue exc = JS_GetException(slot->ctx);
        const char *msg = JS_ToCString(slot->ctx, exc);
        snprintf(err_buf, err_cap, "JS runtime error: %s", msg ? msg : "interrupted");
        if (msg) JS_FreeCString(slot->ctx, msg);
        JS_FreeValue(slot->ctx, exc);
        JS_FreeValue(slot->ctx, ret);
        return -3;
    }

    size_t out_len = 0;
    uint8_t *out_ptr = JS_GetUint8Array(slot->ctx, &out_len, ret);
    if (!out_ptr || out_len == 0 || out_len > res_cap) {
        snprintf(err_buf, err_cap, "evalScan did not return bounded Uint8Array (len=%zu)", out_len);
        JS_FreeValue(slot->ctx, ret);
        return -4;
    }
    memcpy(res_out, out_ptr, out_len);
    JS_FreeValue(slot->ctx, ret);
    return (int32_t)out_len;
}

void plc_qjs_abort(PlcQjsSlot *slot) {
    if (slot) slot->abort_flag = 1;
}

void plc_qjs_free(PlcQjsSlot *slot) {
    if (!slot) return;
    if (slot->ctx) {
        JS_FreeValue(slot->ctx, slot->eval_fn);
        JS_FreeContext(slot->ctx);
    }
    if (slot->rt) JS_FreeRuntime(slot->rt);
    free(slot);
}
