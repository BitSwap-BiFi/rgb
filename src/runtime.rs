// RGB wallet library for smart contracts on Bitcoin & Lightning network
//
// SPDX-License-Identifier: Apache-2.0
//
// Written in 2019-2023 by
//     Dr Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// Copyright (C) 2019-2023 LNP/BP Standards Association. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(clippy::result_large_err)]

use std::convert::Infallible;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::{fs, io};

use amplify::IoError;
use bpstd::{AddressNetwork, Network, XpubDerivable};
use bpwallet::Wallet;
use rgbfs::StockFs;
use rgbstd::containers::{Contract, LoadError, Transfer, XchainOutpoint};
use rgbstd::interface::{BuilderError, OutpointFilter};
use rgbstd::persistence::{Inventory, InventoryDataError, InventoryError, StashError, Stock};
use rgbstd::resolvers::ResolveHeight;
use rgbstd::validation::{self, ResolveTx};
use strict_types::encoding::{DeserializeError, Ident, SerializeError};

use crate::{DescriptorRgb, RgbDescr};

#[derive(Debug, Display, Error, From)]
#[display(inner)]
pub enum RuntimeError {
    #[from]
    #[from(io::Error)]
    Io(IoError),

    #[from]
    Serialize(SerializeError),

    #[from]
    Deserialize(DeserializeError),

    #[from]
    Load(LoadError),

    #[from]
    Stash(StashError<Infallible>),

    #[from]
    #[from(InventoryDataError<Infallible>)]
    Inventory(InventoryError<Infallible>),

    #[from]
    Builder(BuilderError),

    #[from]
    PsbtDecode(psbt::DecodeError),

    /// wallet with id '{0}' is not known to the system.
    #[display(doc_comments)]
    WalletUnknown(Ident),

    #[from]
    InvalidConsignment(validation::Status),

    /// invalid identifier.
    #[from]
    #[display(doc_comments)]
    InvalidId(baid58::Baid58ParseError),

    /// the contract source doesn't fit requirements imposed by the used schema.
    ///
    /// {0}
    #[display(doc_comments)]
    IncompleteContract(validation::Status),

    #[from]
    #[from(bpwallet::LoadError)]
    Bp(bpwallet::RuntimeError),

    #[cfg(feature = "esplora")]
    #[from]
    Esplora(esplora::Error),

    #[from]
    Yaml(serde_yaml::Error),

    #[from]
    Custom(String),
}

impl From<Infallible> for RuntimeError {
    fn from(_: Infallible) -> Self { unreachable!() }
}

#[derive(Getters)]
pub struct Runtime<D: DescriptorRgb<K> = RgbDescr, K = XpubDerivable> {
    stock_path: PathBuf,
    #[getter(as_mut)]
    stock: Stock,
    #[getter(as_mut)]
    wallet: Wallet<K, D /* TODO: Add layer 2 */>,
    #[getter(as_copy)]
    network: Network,
}

impl<D: DescriptorRgb<K>, K> Deref for Runtime<D, K> {
    type Target = Stock;

    fn deref(&self) -> &Self::Target { &self.stock }
}

impl<D: DescriptorRgb<K>, K> DerefMut for Runtime<D, K> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.stock }
}

impl<D: DescriptorRgb<K>, K> OutpointFilter for Runtime<D, K> {
    fn include_output(&self, output: impl Into<XchainOutpoint>) -> bool {
        let output = output.into();
        self.wallet
            .coins()
            .any(|utxo| XchainOutpoint::Bitcoin(utxo.outpoint) == output)
    }
}

#[cfg(feature = "serde")]
impl<D: DescriptorRgb<K>, K> Runtime<D, K>
where
    for<'de> D: serde::Serialize + serde::Deserialize<'de>,
    for<'de> bpwallet::WalletDescr<K, D>: serde::Serialize + serde::Deserialize<'de>,
{
    pub fn load(
        data_dir: PathBuf,
        wallet_name: &str,
        network: Network,
    ) -> Result<Self, RuntimeError> {
        let mut wallet_path = data_dir.clone();
        wallet_path.push(wallet_name);
        let bprt =
            bpwallet::Runtime::<D, K>::load_standard(wallet_path /* TODO: Add layer2 */)?;
        Self::load_attach_or_init(data_dir, network, bprt.detach(), |_| {
            Ok::<_, RuntimeError>(default!())
        })
    }

    pub fn load_attach(
        data_dir: PathBuf,
        network: Network,
        bprt: bpwallet::Runtime<D, K>,
    ) -> Result<Self, RuntimeError> {
        Self::load_attach_or_init(data_dir, network, bprt.detach(), |_| {
            Ok::<_, RuntimeError>(default!())
        })
    }

    pub fn load_or_init<E>(
        data_dir: PathBuf,
        wallet_name: &str,
        network: Network,
        init_wallet: impl FnOnce(bpwallet::LoadError) -> Result<D, E>,
        init_stock: impl FnOnce(DeserializeError) -> Result<Stock, E>,
    ) -> Result<Self, RuntimeError>
    where
        E: From<DeserializeError>,
        bpwallet::LoadError: From<E>,
        RuntimeError: From<E>,
    {
        let mut wallet_path = data_dir.clone();
        wallet_path.push(network.to_string());
        wallet_path.push(wallet_name);
        let bprt = bpwallet::Runtime::load_standard_or_init(
            wallet_path,
            network,
            init_wallet, /* TODO: Add layer2 */
        )?;
        Self::load_attach_or_init(data_dir, network, bprt.detach(), init_stock)
    }

    pub fn load_attach_or_init<E>(
        mut data_dir: PathBuf,
        network: Network,
        wallet: Wallet<K, D>,
        init: impl FnOnce(DeserializeError) -> Result<Stock, E>,
    ) -> Result<Self, RuntimeError>
    where
        E: From<DeserializeError>,
        RuntimeError: From<E>,
    {
        data_dir.push(network.to_string());

        #[cfg(feature = "log")]
        debug!("Using data directory '{}'", data_dir.display());
        fs::create_dir_all(&data_dir)?;

        let mut stock_path = data_dir.clone();
        stock_path.push("stock.dat");

        let stock = Stock::load(&stock_path).or_else(init)?;

        Ok(Self {
            stock_path,
            stock,
            wallet,
            network,
        })
    }
}

impl<D: DescriptorRgb<K>, K> Runtime<D, K> {
    fn store(&mut self) {
        self.stock
            .store(&self.stock_path)
            .expect("unable to save stock");
        // TODO: self.bprt.store()
        /*
        let wallets_fd = File::create(&self.wallets_path)
            .expect("unable to access wallet file; wallets are not saved");
        serde_yaml::to_writer(wallets_fd, &self.wallets).expect("unable to save wallets");
         */
    }

    pub fn attach(&mut self, wallet: Wallet<K, D>) { self.wallet = wallet }

    pub fn unload(self) {}

    pub fn address_network(&self) -> AddressNetwork { self.network.into() }

    pub fn import_contract<R: ResolveHeight>(
        &mut self,
        contract: Contract,
        resolver: &mut R,
    ) -> Result<validation::Status, RuntimeError>
    where
        R::Error: 'static,
    {
        self.stock
            .import_contract(contract, resolver)
            .map_err(RuntimeError::from)
    }

    pub fn validate_transfer(
        &mut self,
        transfer: Transfer,
        resolver: &mut impl ResolveTx,
    ) -> Result<Transfer, RuntimeError> {
        transfer
            .validate(resolver, self.network.is_testnet())
            .map_err(|invalid| invalid.validation_status().expect("just validated").clone())
            .map_err(RuntimeError::from)
    }

    pub fn accept_transfer<R: ResolveHeight>(
        &mut self,
        transfer: Transfer,
        resolver: &mut R,
        force: bool,
    ) -> Result<validation::Status, RuntimeError>
    where
        R::Error: 'static,
    {
        self.stock
            .accept_transfer(transfer, resolver, force)
            .map_err(RuntimeError::from)
    }
}

impl<D: DescriptorRgb<K>, K> Drop for Runtime<D, K> {
    fn drop(&mut self) { self.store() }
}