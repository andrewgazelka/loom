# Read-only runtime closure, isolated admission directory, and DNS configuration.
# Do not mount the entire store: it can contain unrelated private source inputs.
mounts=()
while IFS= read -r dependency; do mounts+=(--ro-bind "$dependency" "$dependency"); done < '@closure@'
for configuration in /etc/resolv.conf /etc/hosts /etc/nsswitch.conf; do
  if [[ -f $configuration ]]; then mounts+=(--ro-bind "$configuration" "$configuration"); fi
done
exec '@bwrap@' --die-with-parent --new-session --unshare-all --share-net \
  --proc /proc --dev /dev --tmpfs /tmp "${mounts[@]}" \
  --bind "$LOOM_IMPORT_ROOT" "$LOOM_IMPORT_ROOT" --chdir "$LOOM_IMPORT_ROOT" \
  --clearenv --setenv DENO_DIR "$DENO_DIR" --setenv HOME "$LOOM_IMPORT_ROOT" \
  --setenv DENO_NO_UPDATE_CHECK 1 --setenv DENO_NO_PROMPT 1 \
  --setenv SSL_CERT_FILE '@certificates@' '@deno@' "$@"
