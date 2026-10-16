import { useEffect } from "react";
import { ProviderIcon } from "@/components/ProviderIcon";
import { TruncatedText } from "@/components/TruncatedText";
import { Switch } from "@/components/ui/switch";
import type { ProviderInfo } from "@/lib/types";
import { m } from "@/paraglide/messages.js";
import { cn } from "@/lib/utils";
import { providerIconId, providerState, sendCommand } from "../model.js";

interface ProviderCatalogProps {
  error: string;
  providers: ProviderInfo[];
}

/** Every built-in provider from the host catalog: on/off in config.toml, and how to sign in. */
export function ProviderCatalog({ error, providers }: ProviderCatalogProps) {
  // The host re-reads credentials only when asked, so ask whenever the list opens.
  useEffect(() => {
    sendCommand("providers");
  }, []);
  if (providers.length === 0) return null;
  return (
    <div className="flex flex-col gap-[var(--header-card-gap)]">
      <div className="flex items-center justify-between gap-2">
        <div className="section-title">{m.all_providers()}</div>
        <button type="button" className="text-[length:var(--sz-badge)] text-[var(--accent)]" onClick={() => sendCommand("detect")}>
          {m.detect_providers()}
        </button>
      </div>
      <div className="card-surface">
        {providers.map((provider) => (
          <ProviderCatalogRow key={provider.id} provider={provider} />
        ))}
      </div>
      <div className={cn("px-[var(--card-pad)] text-[length:var(--sz-badge)]", error ? "text-meter-red" : "text-label-2")}>
        {error || m.all_providers_hint()}
      </div>
    </div>
  );
}

function ProviderCatalogRow({ provider }: { provider: ProviderInfo }) {
  const state = providerState(provider);
  const status =
    state === "off" ? m.provider_off()
      : state === "off-ready" ? m.provider_off_ready()
        : state === "ready" ? m.provider_ready()
          : provider.signIn;
  const action =
    provider.action === "terminal" ? m.provider_sign_in()
      : provider.action === "app" ? m.provider_open_app()
        : provider.action === "settings" ? m.provider_add_key()
          : "";
  const addLabels = { code: m.add_claude_code_account, desktop: m.add_claude_desktop_account, codex: m.add_codex_account };
  return (
    <div data-provider-id={provider.id}>
    <div className={cn("flex items-center gap-[var(--row-gap)] px-[var(--card-pad)] py-[var(--pad-control)]", !provider.enabled && "opacity-55")}>
      <ProviderIcon className="text-label-2" size="var(--row-icon-box)" slug={providerIconId(provider.id)} title={provider.name} />
      <span className="flex min-w-0 flex-1 flex-col">
        <TruncatedText className="text-[length:var(--sz-header)] font-semibold">{provider.name}</TruncatedText>
        <span className={cn("text-[length:var(--sz-badge)] break-words", state === "needs" ? "text-meter-yellow" : "text-label-2")}>{status}</span>
      </span>
      {state === "needs" && action ? (
        <button type="button" className="action-btn" onClick={() => sendCommand("sign-in-provider", { id: provider.id })}>
          {action}
        </button>
      ) : null}
      <Switch
        checked={provider.enabled}
        aria-label={`${m.provider_enable()} ${provider.name}`}
        onCheckedChange={(on) => sendCommand("set-provider-enabled", { id: provider.id, value: on === true })}
      />
    </div>
    {provider.enabled && provider.addAccounts.length > 0 ? (
      <div className="flex flex-wrap gap-x-[var(--row-gap)] px-[var(--card-pad)] pb-[var(--pad-control)] pl-[calc(var(--card-pad)+var(--row-icon-box)+var(--row-gap))]">
        {provider.addAccounts.map((kind) => (
          <button key={kind} type="button" className="text-[length:var(--sz-badge)] text-[var(--accent)]" onClick={() => sendCommand("add-account", { kind })}>
            {addLabels[kind]()}
          </button>
        ))}
      </div>
    ) : null}
    </div>
  );
}
