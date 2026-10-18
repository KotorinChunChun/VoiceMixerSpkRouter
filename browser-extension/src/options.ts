import { loadSettings, saveSettings, type CommonSettings } from "./protocol";

const form = document.querySelector<HTMLFormElement>("#settings")!;
const output = form.querySelector("output")!;
const fields = form.querySelector("fieldset")!;
const saved = await loadSettings();
for (const [key, value] of Object.entries(saved)) {
  const input = form.elements.namedItem(key) as HTMLInputElement;
  if (input.type === "checkbox") input.checked = Boolean(value); else input.value = String(value);
}
form.addEventListener("submit", event => {
  event.preventDefault();
  const values: CommonSettings = {
    port: Number((form.elements.namedItem("port") as HTMLInputElement).value),
    token: (form.elements.namedItem("token") as HTMLInputElement).value
  };
  fields.disabled = true;
  void saveSettings(values)
    .then(() => { output.textContent = "保存しました。各タブの次回開始・再開時に適用します。"; })
    .catch(error => { output.textContent = `保存に失敗しました: ${error instanceof Error ? error.message : String(error)}`; })
    .finally(() => { fields.disabled = false; });
});
fields.disabled = false;
output.textContent = "";
