use anyhow::{Context as _, Result};
use client::proto::{
    self, DapChecksum, DapChecksumAlgorithm, DapEvaluateContext, DapModule, DapScope,
    DapScopePresentationHint, DapSource, DapSourcePresentationHint, DapStackFrame,
    DapStackPresentationHint, DapVariable, DapVariablePresentationHint,
    DapVariablePresentationHintAttributes, DapVariablePresentationHintKind,
    DapVariablePresentationHintVisibility,
};
use dap_types::{OutputEventCategory, OutputEventGroup, ScopePresentationHint, Source};

pub trait ProtoConversion {
    type ProtoType;
    type Output;

    fn to_proto(self) -> Self::ProtoType;
    fn from_proto(payload: Self::ProtoType) -> Self::Output;
}

impl<T> ProtoConversion for Vec<T>
where
    T: ProtoConversion<Output = T>,
{
    type ProtoType = Vec<T::ProtoType>;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        self.into_iter().map(|item| item.to_proto()).collect()
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        payload
            .into_iter()
            .map(|item| T::from_proto(item))
            .collect()
    }
}

impl ProtoConversion for dap_types::Scope {
    type ProtoType = DapScope;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            name: self.name,
            presentation_hint: self.presentation_hint.map(|hint| hint.to_proto().into()),
            variables_reference: self.variables_reference,
            named_variables: self.named_variables,
            indexed_variables: self.indexed_variables,
            expensive: self.expensive,
            source: self.source.map(Source::to_proto),
            line: self.line,
            end_line: self.end_line,
            column: self.column,
            end_column: self.end_column,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        let presentation_hint = payload
            .presentation_hint
            .and_then(DapScopePresentationHint::from_i32);
        Self {
            name: payload.name,
            presentation_hint: presentation_hint.map(ScopePresentationHint::from_proto),
            variables_reference: payload.variables_reference,
            named_variables: payload.named_variables,
            indexed_variables: payload.indexed_variables,
            expensive: payload.expensive,
            source: payload.source.map(dap_types::Source::from_proto),
            line: payload.line,
            end_line: payload.end_line,
            column: payload.column,
            end_column: payload.end_column,
        }
    }
}

impl ProtoConversion for dap_types::Variable {
    type ProtoType = DapVariable;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            name: self.name,
            value: self.value,
            r#type: self.type_,
            presentation_hint: self.presentation_hint.map(|hint| hint.to_proto()),
            evaluate_name: self.evaluate_name,
            variables_reference: self.variables_reference,
            named_variables: self.named_variables,
            indexed_variables: self.indexed_variables,
            memory_reference: self.memory_reference,
            declaration_location_reference: self.declaration_location_reference,
            value_location_reference: self.value_location_reference,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            name: payload.name,
            value: payload.value,
            type_: payload.r#type,
            evaluate_name: payload.evaluate_name,
            presentation_hint: payload
                .presentation_hint
                .map(dap_types::VariablePresentationHint::from_proto),
            variables_reference: payload.variables_reference,
            named_variables: payload.named_variables,
            indexed_variables: payload.indexed_variables,
            memory_reference: payload.memory_reference,
            declaration_location_reference: payload.declaration_location_reference,
            value_location_reference: payload.value_location_reference,
        }
    }
}

impl ProtoConversion for dap_types::VariablePresentationHint {
    type ProtoType = DapVariablePresentationHint;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        DapVariablePresentationHint {
            kind: self.kind.map(|kind| kind.to_proto().into()),
            attributes: self
                .attributes
                .unwrap_or_default()
                .into_iter()
                .map(|attribute| attribute.to_proto().into())
                .collect(),
            visibility: self.visibility.map(|visibility| visibility.to_proto().into()),
            lazy: self.lazy,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        let attributes = payload
            .attributes
            .into_iter()
            .filter_map(DapVariablePresentationHintAttributes::from_i32)
            .map(dap_types::VariablePresentationHintAttributes::from_proto)
            .collect::<Vec<_>>();

        Self {
            kind: payload
                .kind
                .and_then(DapVariablePresentationHintKind::from_i32)
                .map(dap_types::VariablePresentationHintKind::from_proto),
            attributes: (!attributes.is_empty()).then_some(attributes),
            visibility: payload
                .visibility
                .and_then(DapVariablePresentationHintVisibility::from_i32)
                .map(dap_types::VariablePresentationHintVisibility::from_proto),
            lazy: payload.lazy,
        }
    }
}

impl ProtoConversion for dap_types::VariablePresentationHintKind {
    type ProtoType = DapVariablePresentationHintKind;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Property => Self::ProtoType::VariableProperty,
            Self::Method => Self::ProtoType::VariableMethod,
            Self::Class => Self::ProtoType::VariableClass,
            Self::Data => Self::ProtoType::VariableData,
            Self::Event => Self::ProtoType::VariableEvent,
            Self::BaseClass => Self::ProtoType::VariableBaseClass,
            Self::InnerClass => Self::ProtoType::VariableInnerClass,
            Self::Interface => Self::ProtoType::VariableInterface,
            Self::MostDerivedClass => Self::ProtoType::VariableMostDerivedClass,
            Self::Virtual => Self::ProtoType::VariableVirtual,
            Self::DataBreakpoint => Self::ProtoType::VariableDataBreakpoint,
            Self::Unknown => Self::ProtoType::VariableKindUnknown,
            _ => Self::ProtoType::VariableKindUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::VariableProperty => Self::Property,
            Self::ProtoType::VariableMethod => Self::Method,
            Self::ProtoType::VariableClass => Self::Class,
            Self::ProtoType::VariableData => Self::Data,
            Self::ProtoType::VariableEvent => Self::Event,
            Self::ProtoType::VariableBaseClass => Self::BaseClass,
            Self::ProtoType::VariableInnerClass => Self::InnerClass,
            Self::ProtoType::VariableInterface => Self::Interface,
            Self::ProtoType::VariableMostDerivedClass => Self::MostDerivedClass,
            Self::ProtoType::VariableVirtual => Self::Virtual,
            Self::ProtoType::VariableDataBreakpoint => Self::DataBreakpoint,
            Self::ProtoType::VariableKindUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::VariablePresentationHintAttributes {
    type ProtoType = DapVariablePresentationHintAttributes;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Static => Self::ProtoType::VariableStatic,
            Self::Constant => Self::ProtoType::VariableConstant,
            Self::ReadOnly => Self::ProtoType::VariableReadOnly,
            Self::RawString => Self::ProtoType::VariableRawString,
            Self::HasObjectId => Self::ProtoType::VariableHasObjectId,
            Self::CanHaveObjectId => Self::ProtoType::VariableCanHaveObjectId,
            Self::HasSideEffects => Self::ProtoType::VariableHasSideEffects,
            Self::HasDataBreakpoint => Self::ProtoType::VariableHasDataBreakpoint,
            Self::Unknown => Self::ProtoType::VariableAttributeUnknown,
            _ => Self::ProtoType::VariableAttributeUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::VariableStatic => Self::Static,
            Self::ProtoType::VariableConstant => Self::Constant,
            Self::ProtoType::VariableReadOnly => Self::ReadOnly,
            Self::ProtoType::VariableRawString => Self::RawString,
            Self::ProtoType::VariableHasObjectId => Self::HasObjectId,
            Self::ProtoType::VariableCanHaveObjectId => Self::CanHaveObjectId,
            Self::ProtoType::VariableHasSideEffects => Self::HasSideEffects,
            Self::ProtoType::VariableHasDataBreakpoint => Self::HasDataBreakpoint,
            Self::ProtoType::VariableAttributeUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::VariablePresentationHintVisibility {
    type ProtoType = DapVariablePresentationHintVisibility;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Public => Self::ProtoType::VariablePublic,
            Self::Private => Self::ProtoType::VariablePrivate,
            Self::Protected => Self::ProtoType::VariableProtected,
            Self::Internal => Self::ProtoType::VariableInternal,
            Self::Final => Self::ProtoType::VariableFinal,
            Self::Unknown => Self::ProtoType::VariableVisibilityUnknown,
            _ => Self::ProtoType::VariableVisibilityUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::VariablePublic => Self::Public,
            Self::ProtoType::VariablePrivate => Self::Private,
            Self::ProtoType::VariableProtected => Self::Protected,
            Self::ProtoType::VariableInternal => Self::Internal,
            Self::ProtoType::VariableFinal => Self::Final,
            Self::ProtoType::VariableVisibilityUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::ScopePresentationHint {
    type ProtoType = DapScopePresentationHint;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Locals => Self::ProtoType::Locals,
            Self::Arguments => Self::ProtoType::Arguments,
            Self::Registers => Self::ProtoType::Registers,
            Self::ReturnValue => Self::ProtoType::ReturnValue,
            Self::Unknown => Self::ProtoType::ScopeUnknown,
            _ => unreachable!(),
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::Locals => Self::Locals,
            Self::ProtoType::Arguments => Self::Arguments,
            Self::ProtoType::Registers => Self::Registers,
            Self::ProtoType::ReturnValue => Self::ReturnValue,
            Self::ProtoType::ScopeUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::SourcePresentationHint {
    type ProtoType = DapSourcePresentationHint;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Normal => Self::ProtoType::SourceNormal,
            Self::Emphasize => Self::ProtoType::Emphasize,
            Self::Deemphasize => Self::ProtoType::Deemphasize,
            Self::Unknown => Self::ProtoType::SourceUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::SourceNormal => Self::Normal,
            Self::ProtoType::Emphasize => Self::Emphasize,
            Self::ProtoType::Deemphasize => Self::Deemphasize,
            Self::ProtoType::SourceUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::Checksum {
    type ProtoType = DapChecksum;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        DapChecksum {
            algorithm: self.algorithm.to_proto().into(),
            checksum: self.checksum,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            algorithm: dap_types::ChecksumAlgorithm::from_proto(payload.algorithm()),
            checksum: payload.checksum,
        }
    }
}

impl ProtoConversion for dap_types::ChecksumAlgorithm {
    type ProtoType = DapChecksumAlgorithm;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Md5 => DapChecksumAlgorithm::Md5,
            Self::Sha1 => DapChecksumAlgorithm::Sha1,
            Self::Sha256 => DapChecksumAlgorithm::Sha256,
            Self::Timestamp => DapChecksumAlgorithm::Timestamp,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::Md5 => Self::Md5,
            Self::ProtoType::Sha1 => Self::Sha1,
            Self::ProtoType::Sha256 => Self::Sha256,
            Self::ProtoType::Timestamp => Self::Timestamp,
            Self::ProtoType::ChecksumAlgorithmUnspecified => unreachable!(),
        }
    }
}

impl ProtoConversion for dap_types::Source {
    type ProtoType = DapSource;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            name: self.name,
            path: self.path,
            source_reference: self.source_reference,
            presentation_hint: self.presentation_hint.map(|hint| hint.to_proto().into()),
            origin: self.origin,
            sources: self.sources.map(|src| src.to_proto()).unwrap_or_default(),
            adapter_data: self
                .adapter_data
                .and_then(|data| serde_json::to_vec(&data).ok()),
            checksums: self.checksums.map(|c| c.to_proto()).unwrap_or_default(),
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            name: payload.name,
            path: payload.path,
            source_reference: payload.source_reference,
            presentation_hint: payload
                .presentation_hint
                .and_then(DapSourcePresentationHint::from_i32)
                .map(dap_types::SourcePresentationHint::from_proto),
            origin: payload.origin,
            sources: Some(Vec::<dap_types::Source>::from_proto(payload.sources)),
            checksums: Some(Vec::<dap_types::Checksum>::from_proto(payload.checksums)),
            adapter_data: payload
                .adapter_data
                .and_then(|value| serde_json::from_slice(&value).ok()),
        }
    }
}

impl ProtoConversion for dap_types::StackFrame {
    type ProtoType = DapStackFrame;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            id: self.id,
            name: self.name.clone(),
            source: self.source.map(|src| src.to_proto()),
            line: self.line,
            column: self.column,
            end_line: self.end_line,
            end_column: self.end_column,
            can_restart: self.can_restart,
            instruction_pointer_reference: self.instruction_pointer_reference,
            module_id: self.module_id.map(|module_id| proto::DapModuleId {
                id: Some(module_id.to_proto()),
            }),
            presentation_hint: self.presentation_hint.map(|hint| hint.to_proto().into()),
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            id: payload.id,
            name: payload.name,
            source: payload.source.map(dap_types::Source::from_proto),
            line: payload.line,
            column: payload.column,
            end_line: payload.end_line,
            end_column: payload.end_column,
            can_restart: payload.can_restart,
            instruction_pointer_reference: payload.instruction_pointer_reference,
            module_id: payload
                .module_id
                .and_then(|module_id| module_id.id)
                .map(dap_types::ModuleId::from_proto),
            presentation_hint: payload
                .presentation_hint
                .and_then(DapStackPresentationHint::from_i32)
                .map(dap_types::StackFramePresentationHint::from_proto),
        }
    }
}

impl ProtoConversion for dap_types::StackFramePresentationHint {
    type ProtoType = DapStackPresentationHint;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Normal => Self::ProtoType::StackNormal,
            Self::Label => Self::ProtoType::Label,
            Self::Subtle => Self::ProtoType::Subtle,
            Self::Deemphasize => Self::ProtoType::Subtle,
            Self::Unknown => Self::ProtoType::StackUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::StackNormal => Self::Normal,
            Self::ProtoType::Label => Self::Label,
            Self::ProtoType::Subtle => Self::Subtle,
            Self::ProtoType::StackUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::ModuleId {
    type ProtoType = proto::dap_module_id::Id;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Number(num) => Self::ProtoType::Number(num),
            Self::String(string) => Self::ProtoType::String(string),
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self::Output {
        match payload {
            Self::ProtoType::Number(num) => Self::Number(num),
            Self::ProtoType::String(string) => Self::String(string),
        }
    }
}

impl ProtoConversion for dap_types::Module {
    type ProtoType = DapModule;
    type Output = Result<Self>;

    fn to_proto(self) -> Self::ProtoType {
        DapModule {
            id: Some(proto::DapModuleId {
                id: Some(self.id.to_proto()),
            }),
            name: self.name,
            path: self.path,
            is_optimized: self.is_optimized,
            is_user_code: self.is_user_code,
            version: self.version,
            symbol_status: self.symbol_status,
            symbol_file_path: self.symbol_file_path,
            date_time_stamp: self.date_time_stamp,
            address_range: self.address_range,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Result<Self> {
        let id = match payload
            .id
            .context("All DapModule proto messages must have an id")?
            .id
            .context("All DapModuleID proto messages must have an id")?
        {
            proto::dap_module_id::Id::String(string) => dap_types::ModuleId::String(string),
            proto::dap_module_id::Id::Number(num) => dap_types::ModuleId::Number(num),
        };

        Ok(Self {
            id,
            name: payload.name,
            path: payload.path,
            is_optimized: payload.is_optimized,
            is_user_code: payload.is_user_code,
            version: payload.version,
            symbol_status: payload.symbol_status,
            symbol_file_path: payload.symbol_file_path,
            date_time_stamp: payload.date_time_stamp,
            address_range: payload.address_range,
        })
    }
}

impl ProtoConversion for dap_types::SteppingGranularity {
    type ProtoType = proto::SteppingGranularity;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Statement => Self::ProtoType::Statement,
            Self::Line => Self::ProtoType::Line,
            Self::Instruction => Self::ProtoType::Instruction,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::Line => Self::Line,
            Self::ProtoType::Instruction => Self::Instruction,
            Self::ProtoType::Statement => Self::Statement,
        }
    }
}

impl ProtoConversion for dap_types::OutputEventCategory {
    type ProtoType = proto::DapOutputCategory;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Console => Self::ProtoType::ConsoleOutput,
            Self::Important => Self::ProtoType::Important,
            Self::Stdout => Self::ProtoType::Stdout,
            Self::Stderr => Self::ProtoType::Stderr,
            _ => Self::ProtoType::Unknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::ConsoleOutput => Self::Console,
            Self::ProtoType::Important => Self::Important,
            Self::ProtoType::Stdout => Self::Stdout,
            Self::ProtoType::Stderr => Self::Stderr,
            Self::ProtoType::Unknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::OutputEvent {
    type ProtoType = proto::DapOutputEvent;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            category: self.category.map(|category| category.to_proto().into()),
            output: self.output.clone(),
            variables_reference: self.variables_reference,
            source: self.source.map(|source| source.to_proto()),
            line: self.line.map(|line| line as u32),
            column: self.column.map(|column| column as u32),
            group: self.group.map(|group| group.to_proto().into()),
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            category: payload
                .category
                .and_then(proto::DapOutputCategory::from_i32)
                .map(OutputEventCategory::from_proto),
            output: payload.output,
            variables_reference: payload.variables_reference,
            source: payload.source.map(Source::from_proto),
            line: payload.line.map(|line| line as u64),
            column: payload.column.map(|column| column as u64),
            group: payload
                .group
                .and_then(proto::DapOutputEventGroup::from_i32)
                .map(OutputEventGroup::from_proto),
            data: None,
            location_reference: None,
        }
    }
}

impl ProtoConversion for dap_types::OutputEventGroup {
    type ProtoType = proto::DapOutputEventGroup;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Start => Self::ProtoType::Start,
            Self::StartCollapsed => Self::ProtoType::StartCollapsed,
            Self::End => Self::ProtoType::End,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::Start => Self::Start,
            Self::ProtoType::StartCollapsed => Self::StartCollapsed,
            Self::ProtoType::End => Self::End,
        }
    }
}

impl ProtoConversion for dap_types::CompletionItem {
    type ProtoType = proto::DapCompletionItem;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        Self::ProtoType {
            label: self.label.clone(),
            text: self.text.clone(),
            detail: self.detail.clone(),
            typ: self
                .type_
                .map(ProtoConversion::to_proto)
                .map(|typ| typ.into()),
            start: self.start,
            length: self.length,
            selection_start: self.selection_start,
            selection_length: self.selection_length,
            sort_text: self.sort_text,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        let completion_type = payload
            .typ
            .and_then(proto::DapCompletionItemType::from_i32)
            .map(dap_types::CompletionItemType::from_proto);

        Self {
            label: payload.label,
            detail: payload.detail,
            sort_text: payload.sort_text,
            text: payload.text.clone(),
            type_: completion_type,
            start: payload.start,
            length: payload.length,
            selection_start: payload.selection_start,
            selection_length: payload.selection_length,
        }
    }
}

impl ProtoConversion for dap_types::EvaluateArgumentsContext {
    type ProtoType = DapEvaluateContext;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Variables => Self::ProtoType::EvaluateVariables,
            Self::Watch => Self::ProtoType::Watch,
            Self::Hover => Self::ProtoType::Hover,
            Self::Repl => Self::ProtoType::Repl,
            Self::Clipboard => Self::ProtoType::Clipboard,
            Self::Unknown => Self::ProtoType::EvaluateUnknown,
            _ => Self::ProtoType::EvaluateUnknown,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::EvaluateVariables => Self::Variables,
            Self::ProtoType::Watch => Self::Watch,
            Self::ProtoType::Hover => Self::Hover,
            Self::ProtoType::Repl => Self::Repl,
            Self::ProtoType::Clipboard => Self::Clipboard,
            Self::ProtoType::EvaluateUnknown => Self::Unknown,
        }
    }
}

impl ProtoConversion for dap_types::CompletionItemType {
    type ProtoType = proto::DapCompletionItemType;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        match self {
            Self::Class => Self::ProtoType::Class,
            Self::Color => Self::ProtoType::Color,
            Self::Constructor => Self::ProtoType::Constructor,
            Self::Customcolor => Self::ProtoType::Customcolor,
            Self::Enum => Self::ProtoType::Enum,
            Self::Field => Self::ProtoType::Field,
            Self::File => Self::ProtoType::CompletionItemFile,
            Self::Function => Self::ProtoType::Function,
            Self::Interface => Self::ProtoType::Interface,
            Self::Keyword => Self::ProtoType::Keyword,
            Self::Method => Self::ProtoType::Method,
            Self::Module => Self::ProtoType::Module,
            Self::Property => Self::ProtoType::Property,
            Self::Reference => Self::ProtoType::Reference,
            Self::Snippet => Self::ProtoType::Snippet,
            Self::Text => Self::ProtoType::Text,
            Self::Unit => Self::ProtoType::Unit,
            Self::Value => Self::ProtoType::Value,
            Self::Variable => Self::ProtoType::Variable,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        match payload {
            Self::ProtoType::Class => Self::Class,
            Self::ProtoType::Color => Self::Color,
            Self::ProtoType::CompletionItemFile => Self::File,
            Self::ProtoType::Constructor => Self::Constructor,
            Self::ProtoType::Customcolor => Self::Customcolor,
            Self::ProtoType::Enum => Self::Enum,
            Self::ProtoType::Field => Self::Field,
            Self::ProtoType::Function => Self::Function,
            Self::ProtoType::Interface => Self::Interface,
            Self::ProtoType::Keyword => Self::Keyword,
            Self::ProtoType::Method => Self::Method,
            Self::ProtoType::Module => Self::Module,
            Self::ProtoType::Property => Self::Property,
            Self::ProtoType::Reference => Self::Reference,
            Self::ProtoType::Snippet => Self::Snippet,
            Self::ProtoType::Text => Self::Text,
            Self::ProtoType::Unit => Self::Unit,
            Self::ProtoType::Value => Self::Value,
            Self::ProtoType::Variable => Self::Variable,
        }
    }
}

impl ProtoConversion for dap_types::Thread {
    type ProtoType = proto::DapThread;
    type Output = Self;

    fn to_proto(self) -> Self::ProtoType {
        proto::DapThread {
            id: self.id,
            name: self.name,
        }
    }

    fn from_proto(payload: Self::ProtoType) -> Self {
        Self {
            id: payload.id,
            name: payload.name,
        }
    }
}
