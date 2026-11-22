/* Native LS2 oracle for the on-device interoperability tests. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <glib.h>
#include <luna-service2/lunaservice-errors.h>
#include <luna-service2/lunaservice.h>

static const char service_name[] = "com.webos.ls2.test.native";
static const char signal_uri[] = "luna://com.webos.ls2.test.native/test/changed";
static const char echo_payload[] = "{ \"word\" : \"Luna\" }";
static const char first_update[] = "{\"value\":1}";
static const char second_update[] = "{\"value\":2}";
static const char signal_payload[] = "{\"value\":42}";

typedef struct {
    GMainLoop *loop;
    const char *expected[2];
    unsigned int received;
    unsigned int count;
    bool permission_denied;
    bool signal_acknowledged;
    bool ok;
} Wait;

static bool check(bool success, LSError *error, const char *operation)
{
    if (!success) {
        fprintf(stderr, "%s\n", operation);
        LSErrorPrint(error, stderr);
    }
    LSErrorFree(error);
    LSErrorInit(error);
    return success;
}

static bool echo(LSHandle *handle, LSMessage *message, void *context)
{
    (void)context;
    LSError error;
    LSErrorInit(&error);
    return check(LSMessageReply(handle, message, LSMessageGetPayload(message), &error),
                 &error, "reply to echo");
}

static bool updates(LSHandle *handle, LSMessage *message, void *context)
{
    (void)context;
    LSError error;
    LSErrorInit(&error);
    return check(LSSubscriptionAdd(handle, "updates", message, &error),
                 &error, "record subscriber")
        && check(LSMessageReply(handle, message, first_update, &error),
                 &error, "send first update")
        && check(LSSubscriptionReply(handle, "updates", second_update, &error),
                 &error, "send second update");
}

static bool cancelled(LSHandle *handle, LSMessage *message, void *context)
{
    (void)handle;
    (void)message;
    (void)context;
    puts("CANCELLED");
    return true;
}

static bool secret(LSHandle *handle, LSMessage *message, void *context)
{
    (void)context;
    puts("SECRET_DISPATCHED");
    LSError error;
    LSErrorInit(&error);
    return check(LSMessageReply(handle, message, "{\"returnValue\":true}", &error),
                 &error, "reply to secret");
}

static LSMethod methods[] = {
    {"echo", echo, LUNA_METHOD_FLAGS_NONE},
    {"updates", updates, LUNA_METHOD_FLAGS_NONE},
    {"secret", secret, LUNA_METHOD_FLAGS_NONE},
    {0},
};

static LSSignal signals[] = {
    {"changed", LUNA_SIGNAL_FLAGS_NONE},
    {0},
};

static gboolean timed_out(gpointer context)
{
    Wait *wait = context;
    fprintf(stderr, "native LS2 did not receive the expected response\n");
    g_main_loop_quit(wait->loop);
    return G_SOURCE_CONTINUE;
}

static bool await(Wait *wait)
{
    guint deadline = g_timeout_add_seconds(5, timed_out, wait);
    g_main_loop_run(wait->loop);
    g_source_remove(deadline);
    return wait->ok;
}

static bool receive_reply(LSHandle *handle, LSMessage *message, void *context)
{
    (void)handle;
    Wait *wait = context;
    if (wait->received >= wait->count) {
        return true;
    }
    const char *payload = LSMessageGetPayload(message);
    bool matches = false;
    if (payload && wait->permission_denied) {
        matches = strstr(payload, "\"returnValue\"") && strstr(payload, "false")
            && (strstr(payload, "Denied") || strstr(payload, "denied")
                || strstr(payload, "Not permitted"));
    } else if (payload) {
        matches = strcmp(payload, wait->expected[wait->received]) == 0;
    }
    if (!matches) {
        fprintf(stderr, "unexpected native reply %s\n", payload ? payload : "(empty)");
        wait->received = wait->count;
        g_main_loop_quit(wait->loop);
        return true;
    }
    if (++wait->received == wait->count) {
        wait->ok = true;
        g_main_loop_quit(wait->loop);
    }
    return true;
}

static bool call_rust(LSHandle *handle, GMainLoop *loop, LSError *error)
{
    Wait echo_wait = {.loop = loop, .expected = {echo_payload}, .count = 1};
    if (!check(LSCallOneReply(handle, "luna://com.webos.ls2.test.rust/test/echo", echo_payload,
                              receive_reply, &echo_wait, NULL, error), error, "call Rust echo")
        || !await(&echo_wait)) {
        return false;
    }

    Wait update_wait = {.loop = loop, .expected = {first_update, second_update}, .count = 2};
    LSMessageToken token;
    if (!check(LSCall(handle, "luna://com.webos.ls2.test.rust/test/updates", "{\"subscribe\":true}",
                      receive_reply, &update_wait, &token, error), error, "subscribe to Rust updates")
        || !await(&update_wait)
        || !check(LSCallCancel(handle, token, error), error, "cancel Rust subscription")) {
        return false;
    }

    if (getenv("LS2_LEGACY_ROLES")) {
        return true;
    }

    Wait deny = {.loop = loop, .count = 1, .permission_denied = true};
    return check(LSCallOneReply(handle, "luna://com.webos.ls2.test.rust/test/secret", "{}",
                                receive_reply, &deny, NULL, error), error, "call Rust secret")
        && await(&deny);
}

static bool receive_clock(LSHandle *handle, LSMessage *message, void *context)
{
    (void)handle;
    Wait *wait = context;
    const char *payload = LSMessageGetPayload(message);
    wait->ok = payload
        && g_regex_match_simple("\"returnValue\"[[:space:]]*:[[:space:]]*true\\b", payload, 0, 0)
        && g_regex_match_simple("\"utc\"[[:space:]]*:[[:space:]]*[0-9]+", payload, 0, 0);
    if (!wait->ok) {
        fprintf(stderr, "unexpected system clock response %s\n", payload ? payload : "(empty)");
    }
    g_main_loop_quit(wait->loop);
    return true;
}

static bool call_clock(LSHandle *handle, GMainLoop *loop, LSError *error)
{
    Wait wait = {.loop = loop};
    return check(LSCallOneReply(handle, "luna://com.palm.systemservice/time/getSystemTime", "{}",
                                receive_clock, &wait, NULL, error), error, "call active system clock")
        && await(&wait);
}

static bool receive_signal(LSHandle *handle, LSMessage *message, void *context)
{
    (void)handle;
    Wait *wait = context;
    const char *payload = LSMessageGetPayload(message);
    if (!wait->signal_acknowledged) {
        if (payload && strstr(payload, "\"returnValue\"") && strstr(payload, "true")) {
            wait->signal_acknowledged = true;
            puts("READY");
        } else {
            fprintf(stderr, "signal registration rejected: %s\n", payload ? payload : "(empty)");
            g_main_loop_quit(wait->loop);
        }
    } else {
        wait->ok = payload && strcmp(payload, signal_payload) == 0;
        if (!wait->ok) {
            fprintf(stderr, "unexpected signal %s\n", payload ? payload : "(empty)");
        }
        g_main_loop_quit(wait->loop);
    }
    return true;
}

static bool watch(LSHandle *handle, GMainLoop *loop, LSError *error)
{
    Wait wait = {.loop = loop};
    LSMessageToken token;
    return check(LSSignalCall(handle, "/test", "changed", receive_signal, &wait, &token, error),
                 error, "register signal listener")
        && await(&wait)
        && check(LSSignalCallCancel(handle, token, error), error, "unregister signal listener");
}

static gboolean emitted(gpointer context)
{
    g_main_loop_quit(context);
    return G_SOURCE_REMOVE;
}

static bool emit(LSHandle *handle, GMainLoop *loop, LSError *error)
{
    if (!check(LSSignalSend(handle, signal_uri, signal_payload, error), error, "emit changed signal")) {
        return false;
    }
    /* Let the GLib loop flush the native library's async send. */
    g_timeout_add(250, emitted, loop);
    g_main_loop_run(loop);
    return true;
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc != 2 || (strcmp(argv[1], "active") && strcmp(argv[1], "serve") && strcmp(argv[1], "client")
                      && strcmp(argv[1], "watch") && strcmp(argv[1], "emit"))) {
        fprintf(stderr, "usage: %s active|serve|client|watch|emit\n", argv[0]);
        return EXIT_FAILURE;
    }

    bool active = !strcmp(argv[1], "active");
    LSError error;
    LSErrorInit(&error);
    LSHandle *handle;
    bool registered = LSRegister(active ? NULL : service_name, &handle, &error);
    if (!registered && active && (error.error_code == LS_ERROR_CODE_PERMISSION
                                   || error.error_code == -EACCES || error.error_code == -EPERM)) {
        LSErrorFree(&error);
        puts("ACTIVE_DENIED");
        return EXIT_SUCCESS;
    }
    if (!check(registered, &error, "register native fixture")) {
        return EXIT_FAILURE;
    }
    GMainLoop *loop = g_main_loop_new(NULL, FALSE);
    bool ok = check(LSGmainAttach(handle, loop, &error), &error, "attach native fixture");
    if (ok) {
        if (!strcmp(argv[1], "serve") || !strcmp(argv[1], "emit")) {
            ok = check(LSRegisterCategory(handle, "/test", methods, signals, NULL, &error), &error,
                       "register native fixture category");
        }
        if (ok && !strcmp(argv[1], "serve")) {
            ok = check(LSSubscriptionSetCancelFunction(handle, cancelled, NULL, &error), &error,
                       "set native subscription cancel callback");
            if (ok) {
                puts("READY");
                g_main_loop_run(loop);
            }
        } else if (ok && !strcmp(argv[1], "client")) {
            ok = call_rust(handle, loop, &error);
        } else if (ok && !strcmp(argv[1], "watch")) {
            ok = watch(handle, loop, &error);
        } else if (ok && active) {
            ok = call_clock(handle, loop, &error);
        } else if (ok) {
            ok = emit(handle, loop, &error);
        }
    }
    ok = check(LSUnregister(handle, &error), &error, "unregister native fixture") && ok;
    g_main_loop_unref(loop);
    LSErrorFree(&error);
    if (ok && active) {
        puts("ACTIVE_OK");
    }
    return ok ? EXIT_SUCCESS : EXIT_FAILURE;
}
