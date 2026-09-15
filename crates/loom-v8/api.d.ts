/** Guest globals supplied by Loom's V8 sandbox. */
declare namespace Loom {
    type Bytes = readonly number[];
    type Json = null | boolean | number | string | readonly Json[] | {readonly [key: string]: Json};
    type Message = Json | {toJSON(): Json};
    interface CapabilityRef {
        readonly cap: Bytes;
        toJSON(): Bytes;
    }
    interface ActorRef extends CapabilityRef {
        send(value: unknown): Promise<unknown>;
        sendBytes(value: Bytes): Promise<unknown>;
        /** Returns a request reference. Its reply arrives as a later message. */
        call(value: unknown, options: {timeoutMs: number}): Promise<string>;
        reply(reference: string, value: unknown): Promise<unknown>;
        sendAfter(ms: number, value: unknown): Promise<unknown>;
        stop(reason: string): Promise<unknown>;
    }
    interface ActorSpec {
        behavior: string;
        init?: unknown;
        durability?: 'local' | 'remote' | 'ephemeral';
        restart?: 'permanent' | 'transient' | 'temporary';
        shutdown?: 'brutal' | 'infinity' | {timeout_ms: number};
        link?: boolean;
        monitor?: boolean;
        type?: 'worker' | 'supervisor';
    }
    interface ProcessRef extends CapabilityRef {
        write(text: string): Promise<unknown>;
        closeStdin(): Promise<unknown>;
        cancel(): Promise<unknown>;
        subscribe(actor: CapabilityRef | Bytes): Promise<unknown>;
    }
    interface ActorHandlers<T = Json> {
        /** Activation after schema setup and before ordinary inbox messages. */
        onStart?(this: void): unknown;
        onMessage(this: void, message: T): unknown;
        /** Best effort on graceful shutdown; unavailable after abrupt host loss. */
        onStop?(this: void, reason: string): unknown;
    }
    interface ContainerSpec {
        image: string;
        network?: 'none' | 'bridge';
        command?: string;
        args?: readonly string[];
        env?: Readonly<Record<string, string>>;
        limits?: {memoryMb?: number; cpus?: number; pids?: number};
        ttlMs?: number;
        subscriber?: CapabilityRef | Bytes;
    }
    interface WebSocketRef extends CapabilityRef {
        send(text: string): Promise<unknown>;
        sendBytes(bytes: Bytes): Promise<unknown>;
        close(code?: number, reason?: string): Promise<unknown>;
    }
    interface SqlBlob { readonly __loomSqlBlob: unique symbol }
    type SqlValue = null | number | string | Bytes;
    interface Sql {
        (query: string, params?: readonly (null | number | string | SqlBlob)[]): Promise<Record<string, SqlValue>[]>;
        blob(bytes: Bytes): SqlBlob;
    }
    interface API {
        /** Low-level descriptor boundary; prefer the typed helpers below. */
        perform(op: string, args: unknown): Promise<unknown>;
        actor<T = Json>(handlers: ActorHandlers<T>): (bytes: Bytes) => unknown;
        readonly actors: {
            /** Wrapping does not grant authority; accept handed-off capabilities first. */
            get(cap: Bytes): ActorRef;
            accept(cap: Bytes): Promise<ActorRef>;
            spawn(spec: ActorSpec): Promise<ActorRef>;
            self(): Promise<ActorRef>;
            /** Resolves a host-published name within this actor's tenant. */
            named(name: string): Promise<ActorRef>;
            sender(): Promise<ActorRef>;
        };
        readonly processes: {
            get(cap: Bytes): ProcessRef;
            named(name: string): Promise<ProcessRef>;
            /** Names select fixed host registrations, never arbitrary commands. */
            spawn(name: string, options?: {subscriber?: CapabilityRef | Bytes}): Promise<ProcessRef>;
        };
        readonly containers: {
            /** Launches through the tenant's host-configured Docker connection. */
            spawn(spec: ContainerSpec): Promise<ProcessRef>;
        };
        readonly websockets: {
            get(cap: Bytes): WebSocketRef;
            sender(): Promise<WebSocketRef>;
            listen(config?: {maxConnections?: number; maxMessageBytes?: number}): Promise<ActorRef>;
        };
        readonly messages: {
            encode(value: unknown): number[];
            decode(bytes: Bytes): Json;
            json<T extends Json>(handler: (message: T) => unknown): (bytes: Bytes) => unknown;
        };
        readonly sql: Sql;
        now(): Promise<number>;
        random(n: number): Promise<number[]>;
    }
}
declare const loom: Loom.API;
