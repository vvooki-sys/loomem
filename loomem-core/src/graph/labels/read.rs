use super::*;

impl GraphStore {
    /// Resolve a requested label only if that exact label has live source support.
    pub fn get_live_entity_by_name(&self, name: &str, stream: &str) -> Result<Option<EntityNode>> {
        let Some(raw) = self.get_entity_by_name(name, stream)? else {
            return Ok(None);
        };
        let Some(entity) = self.project_entity(raw)? else {
            return Ok(None);
        };
        Ok((entity.stream_id == stream && matches_label(&entity, name)).then_some(entity))
    }

    pub fn get_live_entity_by_id(&self, id: &str) -> Result<Option<EntityNode>> {
        let Some(raw) = self.get_entity_by_id(id)? else {
            return Ok(None);
        };
        self.project_entity(raw)
    }

    /// Every returned endpoint label and normalized relation has matching live proof.
    pub fn get_live_neighbors(&self, id: &str) -> Result<Vec<(Edge, EntityNode)>> {
        let Some(entity) = self.get_live_entity_by_id(id)? else {
            return Ok(Vec::new());
        };
        let mut output = Vec::new();
        for (edge, target) in self.get_neighbors(id)? {
            if let Some(neighbor) = self.project_neighbor(&entity, edge, target)? {
                output.push(neighbor);
            }
        }
        Ok(output)
    }

    fn project_entity(&self, mut entity: EntityNode) -> Result<Option<EntityNode>> {
        let mut claims = Vec::new();
        let mut supported = Vec::new();
        for id in &entity.chunk_ids {
            if let Some(chunk) = self.live_chunk(id, &entity.stream_id)? {
                let labels = self.entity_claims(&entity, &chunk)?;
                if !labels.is_empty() {
                    supported.push(id.clone());
                    claims.extend(labels);
                }
            }
        }
        if claims.is_empty() {
            return Ok(None);
        }
        apply_claims(&mut entity, claims);
        entity.chunk_ids = supported;
        Ok(Some(entity))
    }

    fn entity_claims(&self, entity: &EntityNode, chunk: &Chunk) -> Result<Vec<LabelClaim>> {
        let key = label_key(&entity.id, &chunk.id);
        if let Some(claims) = self.read_claim::<LabelClaims>(&key, &chunk.stream)? {
            ensure!(
                claims.version == 1,
                "Unsupported graph contribution version"
            );
            validate_binding(
                &entity.id,
                chunk,
                &claims.owner_id,
                &claims.chunk_id,
                &claims.stream,
            )?;
            if claims.fingerprint != self.support_fingerprint(chunk)? {
                return Ok(Vec::new());
            }
            return Ok(claims
                .claims
                .into_iter()
                .filter(|claim| !claim.name.is_empty() && !claim.entity_type.is_empty())
                .collect());
        }
        // Legacy cache is explicit per-chunk name evidence, never proof of other
        // aggregate aliases or entity type. No literal source-text guessing.
        Ok(self
            .store
            .get_entities(&chunk.id, &chunk.stream)?
            .into_iter()
            .filter(|name| matches_label(entity, name))
            .map(|name| LabelClaim {
                name,
                entity_type: "Unknown".into(),
                aliases: Vec::new(),
            })
            .collect())
    }

    fn live_chunk(&self, id: &str, stream: &str) -> Result<Option<Chunk>> {
        Ok(self
            .store
            .get_chunk(id)?
            .filter(|chunk| chunk.deleted_at.is_none() && chunk.stream == stream))
    }

    fn project_neighbor(
        &self,
        entity: &EntityNode,
        mut edge: Edge,
        raw: EntityNode,
    ) -> Result<Option<(Edge, EntityNode)>> {
        if edge.stream_id != entity.stream_id || !endpoints_match(entity, &edge, &raw) {
            return Ok(None);
        }
        let Some(target) = self.project_entity(raw)? else {
            return Ok(None);
        };
        let relation = relation_label(&edge.relation_type).to_string();
        let mut support = Vec::new();
        for id in &edge.chunk_ids {
            if self.relation_supported(entity, &target, &edge, id, &relation)? {
                support.push(id.clone());
            }
        }
        if support.is_empty() {
            return Ok(None);
        }
        edge.chunk_ids = support;
        edge.relation_type = relation;
        Ok(Some((edge, target)))
    }

    fn relation_supported(
        &self,
        entity: &EntityNode,
        target: &EntityNode,
        edge: &Edge,
        id: &str,
        relation: &str,
    ) -> Result<bool> {
        if !entity.chunk_ids.iter().any(|value| value == id)
            || !target.chunk_ids.iter().any(|value| value == id)
        {
            return Ok(false);
        }
        let Some(chunk) = self.live_chunk(id, &edge.stream_id)? else {
            return Ok(false);
        };
        if let Some(supported) = self.attributed_relation_supported(edge, &chunk, relation)? {
            return Ok(supported);
        }
        self.legacy_relation_supported(entity, target, edge, &chunk, relation)
    }

    fn attributed_relation_supported(
        &self,
        edge: &Edge,
        chunk: &Chunk,
        relation: &str,
    ) -> Result<Option<bool>> {
        if let Some(claims) =
            self.read_claim::<RelationClaims>(&relation_key(&edge.id, &chunk.id), &chunk.stream)?
        {
            ensure!(
                claims.version == 1,
                "Unsupported graph relation contribution version"
            );
            validate_binding(
                &edge.id,
                chunk,
                &claims.owner_id,
                &claims.chunk_id,
                &claims.stream,
            )?;
            return Ok(Some(
                claims.fingerprint == self.support_fingerprint(chunk)?
                    && claims.relations.contains(relation),
            ));
        }
        Ok(None)
    }

    fn legacy_relation_supported(
        &self,
        entity: &EntityNode,
        target: &EntityNode,
        edge: &Edge,
        chunk: &Chunk,
        relation: &str,
    ) -> Result<bool> {
        let (source, destination) = if edge.source_entity_id == entity.id {
            (entity, target)
        } else {
            (target, entity)
        };
        Ok(self
            .store
            .get_relations(&chunk.id, &chunk.stream)?
            .iter()
            .any(|(left, kind, right)| {
                matches_label(source, left)
                    && matches_label(destination, right)
                    && relation_label(kind) == relation
            }))
    }
}

fn matches_label(entity: &EntityNode, label: &str) -> bool {
    let label = label.to_lowercase();
    entity.canonical_name.to_lowercase() == label
        || entity
            .aliases
            .iter()
            .any(|alias| alias.to_lowercase() == label)
}

fn endpoints_match(entity: &EntityNode, edge: &Edge, target: &EntityNode) -> bool {
    (edge.source_entity_id == entity.id && edge.target_entity_id == target.id)
        || (edge.target_entity_id == entity.id && edge.source_entity_id == target.id)
}

fn apply_claims(entity: &mut EntityNode, mut claims: Vec<LabelClaim>) {
    claims.sort_by_key(|claim| {
        (
            claim.name.to_lowercase(),
            claim.name.clone(),
            claim.entity_type.clone(),
        )
    });
    // Stable identity stays UUID-based. Prefer the original display name only
    // while an incoming canonical-name claim independently supports it.
    let preferred = claims
        .iter()
        .position(|claim| claim.name.eq_ignore_ascii_case(&entity.canonical_name))
        .unwrap_or(0);
    let canonical = claims[preferred].clone();
    let mut labels = BTreeSet::new();
    for claim in claims {
        labels.insert(claim.name);
        labels.extend(claim.aliases);
    }
    entity.aliases = labels
        .into_iter()
        .filter(|label| !label.eq_ignore_ascii_case(&canonical.name))
        .collect();
    entity.canonical_name = canonical.name;
    entity.entity_type = canonical.entity_type;
}
