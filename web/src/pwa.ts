export interface ServiceWorkerRegistrar {
  register(scriptURL: string | URL, options?: RegistrationOptions): Promise<unknown>;
}

export function registerServiceWorker(
  registrar: ServiceWorkerRegistrar | null =
    "serviceWorker" in navigator ? navigator.serviceWorker : null,
): void {
  if (!registrar) return;
  void registrar.register("/sw.js", { scope: "/" }).catch(() => {});
}
