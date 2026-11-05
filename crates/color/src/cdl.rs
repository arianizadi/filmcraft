//! ASC CDL v1.2 forward/clamped XML interchange (`.cc`, `.ccc`, `.cdl`).
//! XML structure and SOP/Saturation equations: Academy CLF v3.0 specification, ASC_CDL section,
//! https://docs.acescentral.com/clf/specification/#asc_cdl (the ASC v1.2-compatible Fwd form).
//! This is interchange data; application and the user's chosen color space belong to the host.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

pub const MAX_CDL_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AscCdl {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub slope: [f64; 3],
    pub offset: [f64; 3],
    pub power: [f64; 3],
    pub saturation: f64,
}

impl AscCdl {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > 256 || self.id.chars().any(char::is_control) || self.description.len() > 4096 {
            return Err("CDL id/description is empty or too long".into());
        }
        if self.slope.iter().chain(&self.power).any(|v| !v.is_finite() || *v < 0.0)
            || self.offset.iter().any(|v| !v.is_finite())
            || !self.saturation.is_finite()
            || self.saturation < 0.0
        {
            return Err("CDL values must be finite; slope, power and saturation must be nonnegative".into());
        }
        Ok(())
    }

    pub fn to_xml(&self) -> Result<String, String> {
        self.validate()?;
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
        let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ColorCorrection xmlns=\"urn:ASC:CDL:v1.01\" id=\"{}\">\n", esc(&self.id));
        if !self.description.is_empty() {
            writeln!(out, "  <Description>{}</Description>", esc(&self.description)).map_err(|e| e.to_string())?;
        }
        out.push_str("  <SOPNode>\n");
        for (name, values) in [("Slope", self.slope), ("Offset", self.offset), ("Power", self.power)] {
            writeln!(out, "    <{name}>{:.17} {:.17} {:.17}</{name}>", values[0], values[1], values[2]).map_err(|e| e.to_string())?;
        }
        writeln!(out, "  </SOPNode>\n  <SatNode><Saturation>{:.17}</Saturation></SatNode>\n</ColorCorrection>", self.saturation).map_err(|e| e.to_string())?;
        Ok(out)
    }
}

fn one_child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Result<roxmltree::Node<'a, 'input>, String> {
    let mut matches = node.children().filter(|n| n.is_element() && n.tag_name().name() == name);
    let found = matches.next().ok_or_else(|| format!("CDL missing {name}"))?;
    if matches.next().is_some() {
        return Err(format!("CDL has duplicate {name}"));
    }
    Ok(found)
}

fn triple(node: roxmltree::Node<'_, '_>, name: &str) -> Result<[f64; 3], String> {
    let node = one_child(node, name)?;
    let values = node
        .text()
        .unwrap_or("")
        .split_whitespace()
        .map(|v| v.parse::<f64>().map_err(|_| format!("CDL {name} is not numeric")))
        .collect::<Result<Vec<_>, _>>()?;
    let array = <[f64; 3]>::try_from(values.as_slice()).map_err(|_| format!("CDL {name} needs exactly three numbers"))?;
    Ok(array)
}

pub fn parse_xml(text: &str) -> Result<Vec<AscCdl>, String> {
    if text.len() > MAX_CDL_BYTES {
        return Err("CDL XML exceeds 1 MiB".into());
    }
    let doc = roxmltree::Document::parse_with_options(text, roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: 65_536, ..Default::default() })
        .map_err(|e| format!("CDL XML: {e}"))?;
    let root = doc.root_element();
    if !matches!(root.tag_name().name(), "ColorCorrection" | "ColorCorrectionCollection" | "ColorDecisionList") {
        return Err("expected ColorCorrection, ColorCorrectionCollection or ColorDecisionList".into());
    }
    let mut result = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for node in root.descendants().filter(|n| n.is_element() && n.tag_name().name() == "ColorCorrection") {
        if result.len() >= 256 {
            return Err("at most 256 CDL corrections per file".into());
        }
        if node.attribute("style").is_some_and(|s| s != "Fwd") {
            return Err("only ASC CDL v1.2 Fwd/clamped is supported".into());
        }
        let sop = one_child(node, "SOPNode")?;
        let sat = one_child(node, "SatNode")?;
        let saturation =
            one_child(sat, "Saturation")?.text().ok_or("CDL missing Saturation value")?.trim().parse::<f64>().map_err(|_| "CDL Saturation is not numeric")?;
        let cdl = AscCdl {
            id: node.attribute("id").unwrap_or("correction").to_string(),
            description: node.children().find(|n| n.is_element() && n.tag_name().name() == "Description").and_then(|n| n.text()).unwrap_or("").to_string(),
            slope: triple(sop, "Slope")?,
            offset: triple(sop, "Offset")?,
            power: triple(sop, "Power")?,
            saturation,
        };
        cdl.validate()?;
        if !ids.insert(cdl.id.clone()) {
            return Err("duplicate CDL correction id".into());
        }
        result.push(cdl);
    }
    if result.is_empty() {
        return Err("CDL file has no ColorCorrection".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_roundtrip_is_lossless_and_escapes_metadata() {
        let c = AscCdl {
            id: "take&<\"'".into(),
            description: "Skin <warm> & blue".into(),
            slope: [1.1, 0.95, 1.0],
            offset: [-0.02, 0.0, 0.05],
            power: [1.0, 1.2, 0.8],
            saturation: 1.23,
        };
        let xml = c.to_xml().unwrap();
        assert_eq!(parse_xml(&xml).unwrap(), vec![c]);
        let collection = format!("<ColorCorrectionCollection>{xml}</ColorCorrectionCollection>").replace("<?xml version=\"1.0\" encoding=\"UTF-8\"?>", "");
        assert_eq!(parse_xml(&collection).unwrap().len(), 1);
    }
    #[test]
    fn hostile_xml_is_an_error_without_panicking() {
        let base = AscCdl { id: "x".into(), description: String::new(), slope: [1.0; 3], offset: [0.0; 3], power: [1.0; 3], saturation: 1.0 }.to_xml().unwrap();
        let mut cases = vec![
            String::new(),
            "<wrong/>".into(),
            base.replace("1.00000000000000000 1.00000000000000000 1.00000000000000000", "NaN 1 1"),
            base.replace("<SOPNode>", "<SOPNode/><SOPNode>"),
            base.replace("<Slope>", "<Slope>1 2 "),
            base.replace("<ColorCorrection ", "<ColorCorrection style=\"FwdNoClamp\" "),
            "<!DOCTYPE x [<!ENTITY x 'no'>]><ColorCorrection id='x'>&x;</ColorCorrection>".into(),
        ];
        for length in (0..base.len()).step_by(7) {
            if base.is_char_boundary(length) {
                cases.push(base.get(..length).unwrap().to_string());
            }
        }
        for text in cases {
            assert!(std::panic::catch_unwind(|| parse_xml(&text)).unwrap().is_err());
        }
    }
}
