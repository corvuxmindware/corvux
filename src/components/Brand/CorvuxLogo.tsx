import { splitProps, type JSX } from "solid-js";
import { settingsStore } from "../../stores/settings";
import { isLightBackgroundTheme } from "../../lib/theme-kind";

type LogoVariant = "auto" | "dark" | "light";

interface CorvuxLogoProps extends Omit<JSX.ImgHTMLAttributes<HTMLImageElement>, "src"> {
  /** "dark" is the light graphite mark for dark UI surfaces. */
  variant?: LogoVariant;
}

export default function CorvuxLogo(props: CorvuxLogoProps) {
  const [local, imgProps] = splitProps(props, ["variant", "alt", "draggable"]);

  const src = () => {
    const variant = local.variant ?? "auto";
    if (variant === "dark") return "/corvux-logo-dark.png";
    if (variant === "light") return "/corvux-logo-light.png";
    // Light-background themes (Daylight, Parchment) need the black mark;
    // dark-background themes use the white one. The list lives in
    // lib/theme-kind so the logo, native controls and diagrams agree.
    return isLightBackgroundTheme(settingsStore.theme())
      ? "/corvux-logo-light.png"
      : "/corvux-logo-dark.png";
  };

  return (
    <img
      {...imgProps}
      src={src()}
      alt={local.alt ?? "Corvux"}
      draggable={local.draggable ?? false}
    />
  );
}
